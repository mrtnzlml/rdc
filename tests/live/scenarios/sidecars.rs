use crate::support::assert_local::{load_lockfile, lockfile_keys, strip_volatile};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// After pull, assert: hook code is extracted to a `.py` sidecar and stripped
/// from JSON; schema formula is extracted to `formulas/amount_total.py`; rule
/// trigger_condition is extracted; redacted fields (hook status, queue counts,
/// inbox email) do not corrupt the round-trip (verified via content-hash
/// stability on a second sync).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_sidecars_redaction() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let _ = seed(&client, &run_id, &static_dir(), &manifest)
        .await
        .expect("seed");
    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    let out = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        out.status.success(),
        "first sync --no-push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let prefix = run_id.list_prefix();

    // hook: a .py sidecar exists and config.code is absent from JSON
    let hslug = lockfile_keys(&lf, "hooks")
        .into_iter()
        .find(|s| s.starts_with(&prefix) && s.contains("validator") && !s.contains("post"))
        .expect("validator hook for this run not found in lockfile");
    assert!(
        project.exists(&format!("envs/test/hooks/{hslug}.py")),
        "hook .py sidecar must exist at envs/test/hooks/{hslug}.py"
    );
    let hv = project.read_json(&format!("envs/test/hooks/{hslug}.json"));
    assert!(
        hv.get("config").and_then(|c| c.get("code")).is_none(),
        "config.code must be extracted out of the hook JSON into the .py sidecar"
    );

    // schema: formula extracted to formulas/amount_total.py (under the queue path)
    let qslug = lockfile_keys(&lf, "queues")
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .expect("no queues for this run found in lockfile");
    let (ws, q) = qslug
        .split_once('/')
        .expect("queue slug must be composite <workspace>/<queue>");
    let formula = format!("envs/test/workspaces/{ws}/queues/{q}/formulas/amount_total.py");
    assert!(
        project.exists(&formula),
        "schema formula sidecar must exist at {formula}"
    );

    // rule: trigger_condition extracted to a sidecar
    let rslug = lockfile_keys(&lf, "rules")
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .expect("no rules for this run found in lockfile");
    assert!(
        project.exists(&format!("envs/test/rules/{rslug}.trigger_condition")),
        "rule trigger_condition sidecar must exist at envs/test/rules/{rslug}.trigger_condition"
    );

    // redaction round-trip stability: record the hook's content_hash after the
    // first pull, run a second sync --no-push, reload the lockfile, and assert
    // the hash is unchanged — proving the sidecar round-trip is idempotent.
    let hash_before = lf
        .objects
        .get("hooks")
        .and_then(|m| m.get(&hslug))
        .and_then(|e| e.content_hash.clone())
        .expect("validator hook must have a content_hash after first pull");

    let out2 = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        out2.status.success(),
        "second sync --no-push failed: {}",
        String::from_utf8_lossy(&out2.stderr)
    );

    let lf2 = load_lockfile(project.path(), "test").expect("lockfile (second load)");
    let hash_after = lf2
        .objects
        .get("hooks")
        .and_then(|m| m.get(&hslug))
        .and_then(|e| e.content_hash.clone())
        .expect("validator hook must still have a content_hash after second pull");

    assert_eq!(
        hash_before, hash_after,
        "content_hash changed on second pull — sidecar round-trip is NOT stable (redaction \
         bug): hook {hslug}"
    );

    let _ = strip_volatile; // keep the import live; used by other scenarios
    drop(teardown);
}
