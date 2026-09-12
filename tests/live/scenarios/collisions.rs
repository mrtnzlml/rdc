use crate::support::assert_local::{load_lockfile, lockfile_keys, queue_file_path};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::assert_converged;
use crate::support::expected::{capture_mode, load_or_compare, CapturedState};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`.
///
/// See `fake_round_trip_core` (`tests/live/scenarios/round_trip.rs`) for why
/// this refusal exists: `collisions_identity` calls `load_or_compare` against
/// `testdata/live/expected/collisions.toml`, a golden captured from a real
/// organization, and a fake-backed run must never be the one that (re)writes
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_collisions_identity() {
    assert!(
        !capture_mode(),
        "RDC_LIVE_CAPTURE is set: a fake-backed run must never capture a golden. \
         Capture only from the live invocation, e.g. \
         `RDC_LIVE_CAPTURE=1 cargo test --test live -- --ignored live_collisions_identity`."
    );
    let fake = crate::support::fake::FakeOrg::start().await;
    collisions_identity(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_collisions_identity() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    collisions_identity(&cfg).await;
}

/// Seed two workspaces each owning a queue named "Invoices" (+ schema +,
/// for one, an inbox). After pull, pin the lockfile slugs and the on-disk
/// directory layout for the same-named objects. The exact dedup form is
/// CAPTURED, not predicted (see plan Global Constraints).
async fn collisions_identity(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");

    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
    let out = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(out.status.success(), "pull failed: {}", String::from_utf8_lossy(&out.stderr));

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    // run_id.list_prefix() = "rdc-it-<id>-" (lowercase, already)
    let prefix = run_id.list_prefix();

    // capture the two same-named queue slugs + their schemas, run-id stripped
    let mut captured = CapturedState::default();
    for kind in ["queues", "schemas", "inboxes"] {
        let keys: Vec<String> = lockfile_keys(&lf, kind)
            .into_iter()
            .filter(|s| s.starts_with(&prefix))
            .map(|s| s.replace(&prefix, "<id>"))
            .collect();
        captured.lockfile_keys.insert(kind.to_string(), keys);
    }
    // assert the invariant we ARE sure of: exactly two queues exist
    assert_eq!(
        captured.lockfile_keys["queues"].len(),
        2,
        "two same-named queues must both be tracked, got {:?}",
        captured.lockfile_keys["queues"]
    );
    // and both on-disk queue files exist (distinct dirs under their workspaces).
    // Queue slugs are FLAT (globally -2-deduped); the file lives at
    // workspaces/<ws>/queues/<flat_slug>/queue.json, found by walking workspaces.
    for slug in lockfile_keys(&lf, "queues").into_iter().filter(|s| s.starts_with(&prefix)) {
        assert!(
            queue_file_path(project.path(), "test", &slug, "queue.json").is_some(),
            "queue file must exist for slug {slug}"
        );
    }

    // rename one queue on the remote, re-pull, assert the on-disk slug is stable
    let qid = index.id("queue-invoices-main").expect("queue id");
    // Keep the run-id prefix so teardown still matches via list_prefix().
    let renamed = run_id.prefix("Invoices Renamed");
    client.patch_name("queue", qid, &renamed).await.expect("rename");
    let out2 = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(out2.status.success(), "re-pull failed: {}", String::from_utf8_lossy(&out2.stderr));
    let lf2 = load_lockfile(project.path(), "test").expect("lockfile2");
    let slugs_after: Vec<String> = lockfile_keys(&lf2, "queues")
        .into_iter()
        .filter(|s| s.starts_with(&prefix))
        .map(|s| s.replace(&prefix, "<id>"))
        .collect();
    assert_eq!(
        slugs_after, captured.lockfile_keys["queues"],
        "queue slugs must be stable across a remote rename (id-pinned identity)"
    );

    // A remote rename must leave the snapshot settled, not churning: the
    // id-pinned slug stays put, so a following cycle has nothing to do.
    assert_converged(&project, "test", &prefix, "after a remote rename was re-pulled");

    let golden = static_dir().join("expected/collisions.toml");
    load_or_compare(&golden, &captured).expect("collision state matches golden");

    drop(teardown);
}
