use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::expected::{load_or_compare, CapturedState};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// Seed the graph (queue->ws/schema/hook, hook run_after), pull, and assert
/// every cross-ref on disk is a portable `rdc://` ref. Capture the
/// (run-id-stripped) `queue.schema` / `queue.workspace` ref values into
/// `CapturedState.refs` for golden comparison.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_cross_refs() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    // Teardown guard FIRST so a panic anywhere still cleans up.
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let _index = seed(&client, &run_id, &static_dir(), &manifest)
        .await
        .expect("seed");

    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    let out = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        out.status.success(),
        "sync --no-push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    // list_prefix() = "rdc-it-<id>-" (lowercase, trailing dash)
    let prefix = run_id.list_prefix();

    // --- assert queue cross-refs are portable rdc:// on disk ---
    // Use the RAW (unstripped) slug so the on-disk path resolves correctly.
    // Unwrap with a panic message so a format regression fails loudly.
    let qslug_raw = lockfile_keys(&lf, "queues")
        .into_iter()
        .next()
        .expect("at least one queue slug in lockfile");
    let (ws, q) = qslug_raw
        .split_once('/')
        .unwrap_or_else(|| panic!("queue slug '{qslug_raw}' expected ws/queue composite form"));
    let queue_rel = format!("envs/test/workspaces/{ws}/queues/{q}/queue.json");
    let queue_raw = project
        .read_to_string(&queue_rel)
        .unwrap_or_else(|| panic!("queue file must exist at {queue_rel}"));
    let qv: serde_json::Value = serde_json::from_str(&queue_raw)
        .unwrap_or_else(|e| panic!("parsing {queue_rel}: {e}"));

    assert!(
        qv["schema"]
            .as_str()
            .unwrap_or_else(|| panic!("queue.schema must be a string"))
            .starts_with("rdc://schemas/"),
        "queue.schema ref must be portable rdc://schemas/... form, got {:?}",
        qv["schema"]
    );
    assert!(
        qv["workspace"]
            .as_str()
            .unwrap_or_else(|| panic!("queue.workspace must be a string"))
            .starts_with("rdc://workspaces/"),
        "queue.workspace ref must be portable rdc://workspaces/... form, got {:?}",
        qv["workspace"]
    );

    // --- assert hook.run_after[] entries are portable rdc:// on disk ---
    for hslug in lockfile_keys(&lf, "hooks") {
        let hook_rel = format!("envs/test/hooks/{hslug}.json");
        let hook_raw = project
            .read_to_string(&hook_rel)
            .unwrap_or_else(|| panic!("hook file must exist at {hook_rel}"));
        let hv: serde_json::Value = serde_json::from_str(&hook_raw)
            .unwrap_or_else(|e| panic!("parsing {hook_rel}: {e}"));
        if let Some(arr) = hv.get("run_after").and_then(|x| x.as_array()) {
            for r in arr {
                assert!(
                    r.as_str()
                        .unwrap_or_else(|| panic!("run_after entry must be a string"))
                        .starts_with("rdc://hooks/"),
                    "hook run_after ref must be portable rdc://hooks/... form in {hslug}, got {r:?}"
                );
            }
        }
    }

    // --- capture run-id-stripped ref values for golden comparison ---
    // Strip only when storing into CapturedState; use raw slug for path.
    let mut captured = CapturedState::default();
    captured.refs.insert(
        "queue.schema".into(),
        qv["schema"]
            .as_str()
            .expect("queue.schema string")
            .replace(&prefix, "<id>"),
    );
    captured.refs.insert(
        "queue.workspace".into(),
        qv["workspace"]
            .as_str()
            .expect("queue.workspace string")
            .replace(&prefix, "<id>"),
    );

    let golden = static_dir().join("expected/cross_refs.toml");
    load_or_compare(&golden, &captured).expect("cross-ref state matches golden");

    drop(teardown); // explicit: delete everything now (also runs on panic)
}
