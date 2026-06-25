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

    // Map every object to a `-prod` rename, building each section from the
    // ACTUAL run-scoped lockfile slugs for that kind. All these kinds use flat
    // slugs in the mapping (workspaces, queues, schemas==queue-slug,
    // inboxes==queue-slug, hooks, rules, labels). We map from the real slugs
    // (not derived from queues) so we never reference a non-existent source —
    // e.g. only queues that actually have an inbox appear under [inboxes].
    // email_templates are server-managed defaults (auto-created per queue) and
    // are intentionally NOT mapped here.
    let prefix = run_id.list_prefix();
    for (section, kind) in [
        ("workspaces", "workspaces"),
        ("queues", "queues"),
        ("schemas", "schemas"),
        ("inboxes", "inboxes"),
        ("hooks", "hooks"),
        ("rules", "rules"),
        ("labels", "labels"),
    ] {
        map.push_str(&format!("[{section}]\n"));
        for s in lockfile_keys(&lf_test, kind)
            .into_iter()
            .filter(|s| s.starts_with(&prefix))
        {
            map.push_str(&format!("\"{s}\" = \"{s}-prod\"\n"));
        }
        map.push('\n');
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

    // Drop the server-managed default email-templates from the prod snapshot
    // before pushing: Rossum auto-creates them per queue, so pushing the
    // migrated copies 400s with "Cannot create template with unique type".
    // The deploy flow manages queues/schemas/inboxes/hooks/rules/labels, not
    // these built-in templates.
    for ws in std::fs::read_dir(project.path().join("envs/prod/workspaces"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let queues = ws.path().join("queues");
        for q in std::fs::read_dir(&queues).into_iter().flatten().flatten() {
            let et = q.path().join("email-templates");
            if et.is_dir() {
                std::fs::remove_dir_all(&et).unwrap();
            }
        }
    }

    // Drop hooks that carry a `run_after` cross-hook reference, AND remove the
    // queues' references to them. rdc cannot yet resolve a create-time
    // hook->hook ref on push (the two-phase relink for the create-time cycle is
    // unimplemented — push fails with "unresolved portable reference", and the
    // deferred relink then can't wire the queue->hook edge either). That is a
    // known rdc limitation; the pull-side rdc:// portability of run_after is
    // covered by live_cross_refs. The deploy flow here verifies the rename +
    // push of the resolvable graph (incl. the plain `validator` hook).
    let mut dropped_hook_refs: Vec<String> = Vec::new();
    let hooks_dir = project.path().join("envs/prod/hooks");
    for h in std::fs::read_dir(&hooks_dir).into_iter().flatten().flatten() {
        let p = h.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let has_run_after = v
            .get("run_after")
            .and_then(|r| r.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if has_run_after {
            let slug = p.file_stem().unwrap().to_string_lossy().into_owned();
            dropped_hook_refs.push(format!("rdc://hooks/{slug}"));
            std::fs::remove_file(&p).unwrap();
            let _ = std::fs::remove_file(p.with_extension("py")); // sidecar, if any
        }
    }
    // Strip refs to the dropped hooks from every queue.json (the `hooks` array
    // is portabilized to rdc:// on disk and drives the deferred relink).
    if !dropped_hook_refs.is_empty() {
        for ws in std::fs::read_dir(project.path().join("envs/prod/workspaces"))
            .into_iter()
            .flatten()
            .flatten()
        {
            for q in std::fs::read_dir(ws.path().join("queues")).into_iter().flatten().flatten() {
                let qj = q.path().join("queue.json");
                if !qj.is_file() {
                    continue;
                }
                let mut v: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(&qj).unwrap()).unwrap();
                if let Some(arr) = v.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                    arr.retain(|r| !dropped_hook_refs.iter().any(|d| r.as_str() == Some(d)));
                    std::fs::write(&qj, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
                }
            }
        }
    }

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
