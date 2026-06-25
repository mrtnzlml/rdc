use crate::support::assert_local::{field, load_lockfile, lockfile_keys};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::expected::{load_or_compare, CapturedState};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// Full round-trip: seed the graph on the remote, `rdc sync test` pulls it
/// down, assert the local snapshot/lockfile, edit a label locally, push it,
/// and assert the remote reflects the edit. Teardown deletes everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_round_trip_core() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };

    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    // Teardown guard FIRST so a panic anywhere still cleans up.
    let teardown = Teardown::new(
        LiveClient::connect(&cfg).expect("connect (teardown)"),
        run_id.clone(),
    );

    // --- seed remote out-of-band ---
    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest)
        .await
        .expect("seed remote");
    assert!(index.id("queue-invoices-main").is_some());

    // --- pull into a fresh local project ---
    let project = ProjectFixture::init(&cfg, &["test", "prod"]).expect("init project");
    let out = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        out.status.success(),
        "sync --no-push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // --- assert local: lockfile recorded all seeded kinds ---
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let prefix = run_id.list_prefix();
    let strip = |slugs: Vec<String>| -> Vec<String> {
        // strip the run-id-derived prefix so golden files are run-agnostic
        slugs
            .into_iter()
            .map(|s| s.replace(&prefix.to_lowercase(), "<id>"))
            .collect()
    };
    let mut captured = CapturedState::default();
    for kind in ["labels", "workspaces", "queues", "schemas", "inboxes", "hooks", "rules"] {
        let keys = strip(lockfile_keys(&lf, kind));
        captured.lockfile_keys.insert(kind.to_string(), keys);
    }
    // Capture a couple of cross-ref values from the pulled queue-main file.
    // Path is discovered from the lockfile's queue slug — use the RAW (unstripped)
    // slug so the on-disk path resolves correctly; strip only when storing values.
    if let Some(qslug_raw) = lockfile_keys(&lf, "queues").into_iter().next() {
        // qslug_raw is composite "<ws>/<q>" with the real rdc-it-<id>- prefix
        if let Some((ws, q)) = qslug_raw.split_once('/') {
            let rel = format!("envs/test/workspaces/{ws}/queues/{q}/queue.json");
            if let Some(raw) = project.read_to_string(&rel) {
                let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
                if let Some(s) = v.get("schema").and_then(|x| x.as_str()) {
                    captured
                        .refs
                        .insert("queue.schema".into(), s.replace(&prefix.to_lowercase(), "<id>"));
                }
                if let Some(w) = v.get("workspace").and_then(|x| x.as_str()) {
                    captured.refs.insert(
                        "queue.workspace".into(),
                        w.replace(&prefix.to_lowercase(), "<id>"),
                    );
                }
            }
        }
    }
    let golden = static_dir().join("expected/round_trip.toml");
    load_or_compare(&golden, &captured).expect("local state matches golden");

    // --- edit a label locally and push ---
    let label_id = index.id("label-priority").expect("label id");
    // Find the label file (only one label slug under our prefix).
    let lslug = lockfile_keys(&lf, "labels")
        .into_iter()
        .next()
        .expect("a label slug");
    let lrel = format!("envs/test/labels/{lslug}.json");
    let mut label: serde_json::Value =
        serde_json::from_str(&project.read_to_string(&lrel).unwrap()).unwrap();
    label["color"] = serde_json::Value::String("#00ff00".into());
    std::fs::write(
        project.path().join(&lrel),
        serde_json::to_vec_pretty(&label).unwrap(),
    )
    .unwrap();

    let push = project.run_rdc(&["sync", "test"]);
    assert!(
        push.status.success(),
        "push sync failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    // --- assert remote reflects the edit ---
    // Labels have no GET-by-id endpoint; use find_listed_value to locate by id.
    let remote_label = client
        .find_listed_value("label", label_id)
        .await
        .expect("list labels")
        .expect("seeded label must still exist remotely");
    let got_color = field(&remote_label, "color");
    assert_eq!(
        got_color,
        Some(&serde_json::Value::String("#00ff00".into())),
        "pushed label color mismatch: got {got_color:?}",
    );

    drop(teardown); // explicit: delete everything now (also runs on panic)
}
