use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::assert_remote::assert_remote_ref_resolved;
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// Deploy flow: pull `test`, `rdc migrate test prod` (renames every object via
/// an explicit mapping so prod objects don't collide with test in the shared
/// org), `rdc sync prod` to push, then assert the prod lockfile recorded
/// `-prod` slugs and that the pushed queue's schema ref resolved to a real URL
/// on the remote. Teardown cleans BOTH test and prod objects (same run-id
/// prefix; display names are identical — only slugs differ).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_deploy_flow() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    // Teardown guard FIRST — cleans up on panic. One guard covers both test
    // AND prod because both carry the same `rdc-it-<id>-` name prefix.
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let _ = seed(&client, &run_id, &static_dir(), &manifest)
        .await
        .expect("seed");

    // Init project with both envs, then pull test only.
    let project = ProjectFixture::init(&cfg, &["test", "prod"]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        pull.status.success(),
        "sync test --no-push failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );

    // Build test->prod rename mapping from the pulled test lockfile.
    // Keys are flat leaf slugs (verified: flat workspace slug, flat queue-leaf
    // slug for queues/schemas/inboxes, flat slug for hooks/rules/labels).
    let lf_test = load_lockfile(project.path(), "test").expect("test lockfile");
    let mut map = String::from("version = 1\n\n");

    // Workspaces: leaf slug (= the directory name under envs/test/workspaces/).
    map.push_str("[workspaces]\n");
    for ws in workspace_leaf_slugs(&project) {
        map.push_str(&format!("\"{ws}\" = \"{ws}-prod\"\n"));
    }

    // Queues / schemas / inboxes: flat LEAF slug (the <q> part of <ws>/<q>).
    let mut queue_leaves: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for slug in lockfile_keys(&lf_test, "queues") {
        if let Some((_ws, q)) = slug.split_once('/') {
            queue_leaves.insert(q.to_string());
        }
    }
    map.push_str("\n[queues]\n");
    for q in &queue_leaves {
        map.push_str(&format!("\"{q}\" = \"{q}-prod\"\n"));
    }
    map.push_str("\n[schemas]\n");
    for q in &queue_leaves {
        map.push_str(&format!("\"{q}\" = \"{q}-prod\"\n"));
    }
    map.push_str("\n[inboxes]\n");
    for q in &queue_leaves {
        map.push_str(&format!("\"{q}\" = \"{q}-prod\"\n"));
    }

    // Hooks / rules / labels: flat slugs.
    for (kind, lk) in [("hooks", "hooks"), ("rules", "rules"), ("labels", "labels")] {
        map.push_str(&format!("\n[{kind}]\n"));
        for s in lockfile_keys(&lf_test, lk) {
            map.push_str(&format!("\"{s}\" = \"{s}-prod\"\n"));
        }
    }

    std::fs::create_dir_all(project.path().join(".rdc/map")).unwrap();
    std::fs::write(project.path().join(".rdc/map/test-to-prod.toml"), &map).unwrap();

    // migrate (pure local rename) then sync prod (push to remote).
    let mg = project.run_rdc(&["migrate", "test", "prod"]);
    assert!(
        mg.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&mg.stderr)
    );

    let sp = project.run_rdc(&["sync", "prod"]);
    assert!(
        sp.status.success(),
        "sync prod failed: {}",
        String::from_utf8_lossy(&sp.stderr)
    );

    // CORRECTED assertion: verify via prod lockfile + remote ref resolution.
    // migrate renames SLUGS, not display names, so remote objects still carry
    // the original `rdc-it-<id>-` names. Assertions:
    //   1. Prod lockfile has at least one queue slug containing "-prod".
    //   2. That queue's schema cross-ref resolved to a real HTTP URL remotely.
    let lf_prod = load_lockfile(project.path(), "prod").expect("prod lockfile");
    let prod_queue_slugs = lockfile_keys(&lf_prod, "queues");

    let prod_slug = prod_queue_slugs
        .iter()
        .find(|s| s.contains("-prod"))
        .unwrap_or_else(|| {
            panic!(
                "prod lockfile must contain at least one queue slug with '-prod'; got: {:?}",
                prod_queue_slugs
            )
        });

    let prod_queue_id = lf_prod
        .objects
        .get("queues")
        .and_then(|m| m.get(prod_slug))
        .unwrap_or_else(|| panic!("prod lockfile missing entry for queue slug '{prod_slug}'"))
        .id;

    assert_remote_ref_resolved(&client, "queue", prod_queue_id, "schema")
        .await
        .unwrap_or_else(|e| {
            panic!(
                "prod queue {prod_queue_id} (slug '{prod_slug}') schema ref not resolved: {e:#}"
            )
        });

    drop(teardown); // explicit: delete test + prod objects (shared prefix)
}

/// Collect workspace leaf slugs from the pulled test snapshot directory.
/// These are the directory names directly under `envs/test/workspaces/`.
fn workspace_leaf_slugs(project: &ProjectFixture) -> Vec<String> {
    let dir = project.path().join("envs/test/workspaces");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out
}
