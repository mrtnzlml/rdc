use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`. `conflicts_deletes` never calls `capture_mode` /
/// `load_or_compare` — it has no golden — so there is nothing here to refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_conflicts_deletes() {
    let fake = crate::support::fake::FakeOrg::start().await;
    conflicts_deletes(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_conflicts_deletes() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    conflicts_deletes(&cfg).await;
}

/// Deterministic conflict + delete outcomes against the real API:
///
///  (a) content conflict (both-diverged), non-interactive => a shadow file is
///      written and local is kept;
///  (b) `--conflict keep-local` resolves that standing conflict: local wins,
///      it reaches the remote, and the stale shadow is swept;
///  (c) `--conflict use-remote` resolves a fresh conflict the other way:
///      the env's copy overwrites local, and its shadow is swept too;
///  (d) local tombstone + `--allow-deletes` => the object is DELETEd remotely.
///
/// (b) and (c) are the live coverage for `--conflict`, and they also pin the
/// stale-shadow class directly: a shadow left behind after a resolution makes
/// the pull post-pass skip that object *forever*, which is why every branch
/// here ends in a full convergence check rather than just an assertion about
/// the value.
async fn conflicts_deletes(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    // Teardown guard FIRST so a panic anywhere still cleans up.
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest)
        .await
        .expect("seed");

    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        pull.status.success(),
        "initial pull failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let prefix = run_id.list_prefix();

    let lslug = lockfile_keys(&lf, "labels")
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .expect("label slug for this run not found in lockfile");
    let lrel = format!("envs/test/labels/{lslug}.json");
    let shadow = format!(".rdc/conflicts/test/labels/{lslug}.json");
    let lid = index.id("label-priority").expect("label-priority id from seed index");

    // Local helper: set the label's `color` on disk.
    let set_local_color = |color: &str| {
        let mut v: serde_json::Value =
            serde_json::from_str(&project.read_to_string(&lrel).unwrap()).unwrap();
        v["color"] = serde_json::Value::String(color.into());
        std::fs::write(
            project.path().join(&lrel),
            serde_json::to_vec_pretty(&v).unwrap(),
        )
        .unwrap();
    };
    let local_color = || -> String {
        let v: serde_json::Value =
            serde_json::from_str(&project.read_to_string(&lrel).unwrap()).unwrap();
        v["color"].as_str().unwrap_or_default().to_string()
    };

    // -------------------------------------------------------------------------
    // (a) Both-diverged conflict: change local color AND remote color, then sync
    //     non-interactively (piped stdin => auto --yes => shadow file written).
    // -------------------------------------------------------------------------
    // Changing DIFFERENT fields (e.g. local color + remote name) auto-merges on
    // this API and produces no conflict, so the SAME field is required.
    set_local_color("#111111");
    client
        .patch_fields("label", lid, serde_json::json!({ "color": "#00ff00" }))
        .await
        .expect("patch remote label color");

    let confl = project.run_rdc(&["sync", "test"]);
    assert!(
        confl.status.success(),
        "conflict sync must not error in non-interactive mode: {}",
        combined(&confl)
    );

    // The shadow is parked under the gitignored `.rdc/conflicts/<env>/`
    // tree, mirroring the label's env-tree relpath (keeps its normal name).
    assert!(
        project.exists(&shadow),
        "shadow file must be written on a non-interactive content conflict; expected: {shadow}"
    );
    // Local was kept, not silently overwritten by the remote's value.
    assert_eq!(local_color(), "#111111", "local must be kept on a skipped conflict");

    // -------------------------------------------------------------------------
    // (b) `--conflict keep-local` resolves it: local wins and is pushed.
    // -------------------------------------------------------------------------
    let keep = project.run_rdc(&["sync", "test", "--conflict", "keep-local"]);
    assert!(keep.status.success(), "--conflict keep-local failed: {}", combined(&keep));

    let remote_label = client
        .find_listed_value("label", lid)
        .await
        .expect("list labels")
        .expect("seeded label must still exist remotely");
    assert_eq!(
        remote_label.get("color"),
        Some(&serde_json::Value::String("#111111".into())),
        "--conflict keep-local must push the LOCAL value to the env: {remote_label:?}"
    );
    assert!(
        !project.exists(&shadow),
        "the stale shadow must be swept once the conflict is resolved; a leftover \
         shadow makes the pull post-pass skip this object on every later sync"
    );
    assert_converged(&project, "test", &prefix, "after --conflict keep-local");

    // -------------------------------------------------------------------------
    // (c) `--conflict use-remote` resolves a fresh conflict the other way.
    // -------------------------------------------------------------------------
    set_local_color("#222222");
    client
        .patch_fields("label", lid, serde_json::json!({ "color": "#333333" }))
        .await
        .expect("patch remote label color again");

    let use_remote = project.run_rdc(&["sync", "test", "--conflict", "use-remote"]);
    assert!(
        use_remote.status.success(),
        "--conflict use-remote failed: {}",
        combined(&use_remote)
    );
    assert_eq!(
        local_color(),
        "#333333",
        "--conflict use-remote must overwrite local with the env's copy"
    );
    assert!(
        !project.exists(&shadow),
        "--conflict use-remote must also sweep the conflict shadow"
    );
    assert_converged(&project, "test", &prefix, "after --conflict use-remote");

    // -------------------------------------------------------------------------
    // (d) Local tombstone + --allow-deletes => remote DELETE.
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
        project.path().join(format!("envs/test/rules/{rslug}.py")),
    )
    .ok(); // sidecar may not exist for all rule fixtures

    // Without the flag, a non-interactive delete must be REFUSED — the object
    // survives and nothing else in the cycle is applied destructively.
    let refused = project.run_rdc(&["sync", "test"]);
    assert!(
        !refused.status.success(),
        "a non-interactive sync with a pending tombstone must refuse without \
         --allow-deletes: {}",
        combined(&refused)
    );
    assert!(
        !client
            .list_ids_by_name_prefix("rule", &prefix)
            .await
            .expect("list rules after the refusal")
            .is_empty(),
        "the rule must survive a refused delete"
    );

    let del = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(del.status.success(), "delete sync failed: {}", combined(&del));

    // Confirm the rule is gone on the remote.
    let remaining = client
        .list_ids_by_name_prefix("rule", &prefix)
        .await
        .expect("list rules after delete");
    assert!(
        remaining.is_empty(),
        "rule must be deleted on the remote after --allow-deletes; still found: {remaining:?}"
    );
    assert_converged(&project, "test", &prefix, "after deleting the rule");

    drop(teardown);
}
