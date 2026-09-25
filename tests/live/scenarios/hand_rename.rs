use crate::support::assert_local::{load_lockfile, queue_file_path};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`. No golden, so nothing to refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_hand_rename() {
    let fake = crate::support::fake::FakeOrg::start().await;
    hand_rename(&fake.config()).await;
}

/// The live twin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_hand_rename() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    hand_rename(&cfg).await;
}

/// Files renamed by hand keep their objects.
///
/// A file moved to a new slug with its `id` left inside used to read as a
/// delete plus a create: the object was DELETEd on the env and POSTed again
/// with a new id, and a hook came back without its secrets, which are keyed by
/// slug. One sync renames a hook (with a secret, and named by another hook's
/// `run_after`), a rule and a queue directory; nothing may be deleted or
/// created, every id survives under its new slug, the refs and the secret
/// follow, and `assert_converged` passes.
async fn hand_rename(cfg: &LiveConfig) {
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
    assert!(pull.status.success(), "initial pull failed: {}", combined(&pull));

    let prefix = run_id.list_prefix();
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let slug_of = |kind: &str, key: &str| {
        lf.slug_for_id(kind, index.id(key).expect("seeded id"))
            .unwrap_or_else(|| panic!("seeded {key} in the lockfile"))
            .to_string()
    };
    let (validator, post_validator) = (slug_of("hooks", "hook-validator"), slug_of("hooks", "hook-post-validator"));
    let rule = slug_of("rules", "rule-totals");
    let queue = slug_of("queues", "queue-invoices-main");
    let ids = |kind: &str| -> std::collections::BTreeSet<u64> {
        load_lockfile(project.path(), "test")
            .expect("lockfile")
            .objects
            .get(kind)
            .map(|m| m.values().map(|e| e.id).collect())
            .unwrap_or_default()
    };
    let before: Vec<_> = ["hooks", "rules", "queues", "schemas", "email_templates"]
        .iter()
        .map(|k| ids(k))
        .collect();

    // A secret for the hook about to move.
    std::fs::write(
        project.path().join("secrets/test.hook-secrets.json"),
        format!("{{\"hooks\": {{\"{validator}\": {{\"api_key\": \"s3cret\"}}}}}}\n"),
    )
    .unwrap();

    let mv = |from: std::path::PathBuf, to: std::path::PathBuf| std::fs::rename(from, to).unwrap();
    let env = project.path().join("envs/test");
    let (new_hook, new_rule, new_queue) =
        (format!("{prefix}moved-hook"), format!("{prefix}moved-rule"), format!("{prefix}moved-queue"));
    for ext in ["json", "py"] {
        let from = env.join(format!("hooks/{validator}.{ext}"));
        if from.exists() {
            mv(from, env.join(format!("hooks/{new_hook}.{ext}")));
        }
        let from = env.join(format!("rules/{rule}.{ext}"));
        if from.exists() {
            mv(from, env.join(format!("rules/{new_rule}.{ext}")));
        }
    }
    let queue_dir = queue_file_path(project.path(), "test", &queue, "queue.json")
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    mv(queue_dir.clone(), queue_dir.with_file_name(&new_queue));

    // The preview must already show renames, not deletes and creates.
    let dry = combined(&project.run_rdc(&["sync", "test", "--dry-run"]));
    for line in dry.lines() {
        let doomed = [&validator, &rule, &queue].iter().any(|s| line.contains(s.as_str()))
            && line.contains("delete");
        let reborn = [&new_hook, &new_rule, &new_queue].iter().any(|s| line.contains(s.as_str()))
            && line.contains("post");
        assert!(!doomed && !reborn, "the dry run must not plan a delete + create: {line}\n{dry}");
    }

    let (out, tr) = project.run_rdc_traced(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "sync failed: {}", combined(&out));
    for endpoint in ["hooks", "rules", "queues", "schemas", "email_templates", "inboxes"] {
        for m in ["POST", "DELETE"] {
            assert!(
                tr.first(m, endpoint).is_none(),
                "a hand rename must not {m} /{endpoint}:\n{}",
                combined(&out)
            );
        }
    }
    let after: Vec<_> = ["hooks", "rules", "queues", "schemas", "email_templates"]
        .iter()
        .map(|k| ids(k))
        .collect();
    assert_eq!(after, before, "every object must keep its id");

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    for (kind, slug) in [("hooks", &new_hook), ("rules", &new_rule), ("queues", &new_queue), ("schemas", &new_queue)] {
        assert!(
            lf.objects.get(kind).is_some_and(|m| m.contains_key(slug.as_str())),
            "{kind}/{slug} must be the lockfile key after the rename"
        );
    }
    let survivor = project.read_json(&format!("envs/test/hooks/{post_validator}.json"));
    assert_eq!(
        survivor["run_after"],
        serde_json::json!([format!("rdc://hooks/{new_hook}")]),
        "the other hook's run_after must follow the rename"
    );
    let secrets = project.read_json("secrets/test.hook-secrets.json");
    assert_eq!(secrets["hooks"][&new_hook]["api_key"], "s3cret", "the secret must follow: {secrets}");
    assert!(secrets["hooks"].get(&validator).is_none(), "no secret left at the old slug: {secrets}");
    assert_converged(&project, "test", &prefix, "hand-renamed hook, rule and queue");

    drop(teardown);
}
