use crate::support::assert_local::{load_lockfile, lockfile_keys, queue_file_path};
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
async fn fake_backref_refresh() {
    let fake = crate::support::fake::FakeOrg::start().await;
    backref_refresh(&fake.config()).await;
}

/// The live twin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_backref_refresh() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    backref_refresh(&cfg).await;
}

/// A rule create or delete moves the server-derived `queue.rules` back-ref.
/// rdc never authors that array, so the sync that pushes the rule must read
/// the queue back afterwards (`sync::settle_pass`) — otherwise the queue stays
/// stale on disk and only the NEXT sync writes it: one run is not idempotent.
///
/// Each of these cases once took two syncs, because the queue was touched for
/// another reason in the same cycle. (c) was also a push bug: the rule DELETE
/// runs first and moves `queue.rules`, and the pre-PATCH drift check skipped
/// the queue edit as "remote changed".
///
///  (a) the queue is edited locally and a new rule targets it;
///  (b) the queue is edited remotely and a new rule targets it;
///  (c) the queue is edited locally and a rule on it is deleted.
///
/// Each case is one sync followed by `assert_converged`.
async fn backref_refresh(cfg: &LiveConfig) {
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
    // By id: the seed also carries a same-named queue in another workspace.
    let queue_id = index.id("queue-invoices-main").expect("seeded queue id");
    let q_slug = lf
        .slug_for_id("queues", queue_id)
        .expect("the seeded queue in the lockfile")
        .to_string();
    let queue_path = queue_file_path(project.path(), "test", &q_slug, "queue.json")
        .unwrap_or_else(|| panic!("queue.json for {q_slug} on disk"));
    let seeded_rule = lockfile_keys(&lf, "rules")
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .expect("the seeded rule in the lockfile");

    let read_queue = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(&queue_path).unwrap()).unwrap()
    };
    let write_queue = |v: &serde_json::Value| {
        let mut bytes = serde_json::to_vec_pretty(v).unwrap();
        bytes.push(b'\n');
        std::fs::write(&queue_path, bytes).unwrap();
    };
    let edit_queue_locally = |timeout: &str| {
        let mut q = read_queue();
        q["session_timeout"] = serde_json::Value::String(timeout.into());
        write_queue(&q);
    };
    let queue_rules = || -> Vec<String> {
        read_queue()["rules"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    // A new rule on this run's queue, cloned from the pulled seed rule so its
    // shape is whatever rdc writes for this server.
    let add_local_rule = |name: &str| -> String {
        let slug = format!("{prefix}{name}");
        let src = format!("envs/test/rules/{seeded_rule}");
        let mut rule = project.read_json(&format!("{src}.json"));
        let o = rule.as_object_mut().unwrap();
        for k in ["id", "created_at", "created_by", "modified_at", "modified_by"] {
            o.remove(k);
        }
        o.insert("name".into(), serde_json::Value::String(run_id.prefix(name)));
        o.insert("url".into(), serde_json::Value::String(format!("rdc://rules/{slug}")));
        project.write_json(&format!("envs/test/rules/{slug}.json"), &rule);
        if let Some(code) = project.read_to_string(&format!("{src}.py")) {
            std::fs::write(project.path().join(format!("envs/test/rules/{slug}.py")), code)
                .unwrap();
        }
        format!("rdc://rules/{slug}")
    };

    // (a) Local queue edit + a new rule on the same queue.
    edit_queue_locally("02:00:00");
    let rule_a = add_local_rule("extra-a");
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "(a) sync failed: {}", combined(&out));
    assert!(
        queue_rules().contains(&rule_a),
        "(a) the sync that created {rule_a} must also write it into the locally \
         edited queue's `rules` back-ref; got {:?}",
        queue_rules()
    );
    assert_converged(&project, "test", &prefix, "(a) local queue edit + rule create");

    // (b) Remote queue edit + a new rule on the same queue.
    client
        .patch_fields("queue", queue_id, serde_json::json!({ "session_timeout": "03:00:00" }))
        .await
        .expect("patch the queue remotely");
    let rule_b = add_local_rule("extra-b");
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "(b) sync failed: {}", combined(&out));
    assert_eq!(read_queue()["session_timeout"], "03:00:00", "(b) the remote edit must be pulled");
    assert!(
        queue_rules().contains(&rule_b),
        "(b) the sync that created {rule_b} must also write it into the queue it \
         pulled for a remote edit; got {:?}",
        queue_rules()
    );
    assert_converged(&project, "test", &prefix, "(b) remote queue edit + rule create");

    // (c) Local queue edit + a rule on the same queue deleted.
    edit_queue_locally("04:00:00");
    let slug_a = rule_a.trim_start_matches("rdc://rules/");
    for ext in ["json", "py"] {
        let _ = std::fs::remove_file(project.path().join(format!("envs/test/rules/{slug_a}.{ext}")));
    }
    let out = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "(c) sync failed: {}", combined(&out));
    assert!(
        !queue_rules().contains(&rule_a),
        "(c) the sync that deleted {rule_a} must also drop it from the locally \
         edited queue's `rules` back-ref; got {:?}",
        queue_rules()
    );
    assert_converged(&project, "test", &prefix, "(c) local queue edit + rule delete");

    drop(teardown);
}
