use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// Non-interactive (piped stdin => auto --yes) deterministic outcomes:
///  (a) content conflict (both-diverged) => a shadow file is written, local kept;
///  (b) local tombstone + --allow-deletes => the object is DELETEd on the remote.
///
/// LIVE-BEHAVIOR ASSUMPTIONS (maintainer must confirm during first live run):
///  1. That changing local `color` + remote `name` yields a `BothDiverged`
///     classification rather than an auto-merge. If it auto-merges, switch to
///     mutating the SAME field on both sides.
///  2. That the shadow file is written under the non-TTY auto-`--yes` fallthrough
///     rather than the sync erroring. Confirm with `--dry-run` during bring-up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_conflicts_deletes() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    // Teardown guard FIRST so a panic anywhere still cleans up.
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest)
        .await
        .expect("seed");

    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        pull.status.success(),
        "initial pull failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let prefix = run_id.list_prefix();

    // -------------------------------------------------------------------------
    // (a) Both-diverged conflict: change local color AND remote name, then sync
    //     non-interactively (piped stdin => auto --yes => shadow file written).
    // -------------------------------------------------------------------------
    let lslug = lockfile_keys(&lf, "labels")
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .expect("label slug for this run not found in lockfile");

    let lrel = format!("envs/test/labels/{lslug}.json");

    // Mutate the local label's color field.
    let mut local: serde_json::Value =
        serde_json::from_str(&project.read_to_string(&lrel).unwrap()).unwrap();
    local["color"] = serde_json::Value::String("#111111".into());
    std::fs::write(
        project.path().join(&lrel),
        serde_json::to_vec_pretty(&local).unwrap(),
    )
    .unwrap();

    // Mutate the remote label's name field (diverges on a different field).
    let lid = index.id("label-priority").expect("label-priority id from seed index");
    client
        .patch_name("label", lid, &run_id.prefix("Priority Remote"))
        .await
        .expect("patch remote label name");

    // Sync non-interactively; piped stdin triggers auto --yes / shadow fallback.
    let confl = project.run_rdc(&["sync", "test"]);
    assert!(
        confl.status.success(),
        "conflict sync must not error in non-interactive mode: {}",
        String::from_utf8_lossy(&confl.stderr)
    );

    // The exact shadow file path: <full-filename>.<env>, i.e. <slug>.json.test
    let shadow = format!("envs/test/labels/{lslug}.json.test");
    assert!(
        project.exists(&shadow),
        "shadow file must be written on a non-interactive content conflict; expected: {shadow}"
    );

    // -------------------------------------------------------------------------
    // (b) Local tombstone + --allow-deletes => remote DELETE.
    // -------------------------------------------------------------------------
    let rslug = lockfile_keys(&lf, "rules")
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .expect("rule slug for this run not found in lockfile");

    // Remove the rule JSON (and its trigger_condition sidecar if present).
    std::fs::remove_file(
        project.path().join(format!("envs/test/rules/{rslug}.json")),
    )
    .expect("rule json must exist to delete");
    std::fs::remove_file(
        project.path().join(format!("envs/test/rules/{rslug}.trigger_condition")),
    )
    .ok(); // sidecar may not exist for all rule fixtures

    let del = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(
        del.status.success(),
        "delete sync failed: {}",
        String::from_utf8_lossy(&del.stderr)
    );

    // Confirm the rule is gone on the remote.
    let remaining = client
        .list_ids_by_name_prefix("rule", &run_id.list_prefix())
        .await
        .expect("list rules after delete");
    assert!(
        remaining.is_empty(),
        "rule must be deleted on the remote after --allow-deletes; still found: {remaining:?}"
    );

    drop(teardown);
}
