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
async fn fake_settle_after_push() {
    let fake = crate::support::fake::FakeOrg::start().await;
    settle_after_push(&fake.config()).await;
}

/// The live twin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_settle_after_push() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    settle_after_push(&cfg).await;
}

/// A write can change objects the sync never touched, and the sync's first
/// pull reads a listing taken before it pushed. Each case below used to leave
/// work for the NEXT sync; the settle pass (`sync::settle_pass`) reads the env
/// back so one sync is enough:
///
///  (a) a queue created without its email templates declared — the server
///      provisions five, which land on disk in the same run;
///  (b) a hook deleted while another hook runs after it — the server drops it
///      from the survivor's `run_after`;
///  (c) a queue deleted while a rule still targets it — the server keeps a
///      draining queue in the rule's `queues` for up to 24 hours, so rdc
///      detaches it in the same run, and the queue's schema and unique-typed
///      templates, which the server refuses to delete apart from the queue,
///      leave the lockfile in the same run.
///
/// Each case is one sync followed by `assert_converged`.
async fn settle_after_push(cfg: &LiveConfig) {
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
    let seeded_queue = lf
        .slug_for_id("queues", index.id("queue-invoices-main").expect("seeded queue id"))
        .expect("the seeded queue in the lockfile")
        .to_string();
    let seeded_dir = queue_file_path(project.path(), "test", &seeded_queue, "queue.json")
        .expect("seeded queue.json on disk")
        .parent()
        .unwrap()
        .to_path_buf();
    let seeded_rule = lockfile_keys(&lf, "rules")
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .expect("the seeded rule in the lockfile");
    let hook_slug = |key: &str| {
        lf.slug_for_id("hooks", index.id(key).expect("seeded hook id"))
            .expect("seeded hook in the lockfile")
            .to_string()
    };
    let (validator, post_validator) = (hook_slug("hook-validator"), hook_slug("hook-post-validator"));

    let strip_server_fields = |v: &mut serde_json::Value| {
        let o = v.as_object_mut().unwrap();
        for k in ["id", "created_at", "created_by", "modified_at", "modified_by"] {
            o.remove(k);
        }
    };
    let write_pretty = |path: &std::path::Path, v: &serde_json::Value| {
        let mut bytes = serde_json::to_vec_pretty(v).unwrap();
        bytes.push(b'\n');
        std::fs::write(path, bytes).unwrap();
    };

    // (a) A new queue with its schema, but none of the email templates the
    //     server provisions for it — plus a rule on it, used by (c).
    let fresh = format!("{prefix}fresh");
    let fresh_dir = seeded_dir.with_file_name(&fresh);
    std::fs::create_dir_all(fresh_dir.join("formulas")).unwrap();
    let mut queue: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(seeded_dir.join("queue.json")).unwrap())
            .unwrap();
    strip_server_fields(&mut queue);
    queue["name"] = run_id.prefix("Fresh").into();
    queue["url"] = format!("rdc://queues/{fresh}").into();
    queue["schema"] = format!("rdc://schemas/{fresh}").into();
    for backref in ["hooks", "webhooks", "rules", "inbox"] {
        queue.as_object_mut().unwrap().remove(backref);
    }
    write_pretty(&fresh_dir.join("queue.json"), &queue);
    let mut schema: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(seeded_dir.join("schema.json")).unwrap())
            .unwrap();
    strip_server_fields(&mut schema);
    schema["name"] = run_id.prefix("Fresh").into();
    schema["url"] = format!("rdc://schemas/{fresh}").into();
    schema.as_object_mut().unwrap().remove("queues");
    write_pretty(&fresh_dir.join("schema.json"), &schema);
    for f in std::fs::read_dir(seeded_dir.join("formulas")).unwrap().flatten() {
        std::fs::copy(f.path(), fresh_dir.join("formulas").join(f.file_name())).unwrap();
    }
    let fresh_rule = format!("{prefix}on-fresh");
    let mut rule = project.read_json(&format!("envs/test/rules/{seeded_rule}.json"));
    strip_server_fields(&mut rule);
    rule["name"] = run_id.prefix("On fresh").into();
    rule["url"] = format!("rdc://rules/{fresh_rule}").into();
    rule["queues"] = serde_json::json!([format!("rdc://queues/{fresh}")]);
    project.write_json(&format!("envs/test/rules/{fresh_rule}.json"), &rule);
    if let Some(code) = project.read_to_string(&format!("envs/test/rules/{seeded_rule}.py")) {
        std::fs::write(project.path().join(format!("envs/test/rules/{fresh_rule}.py")), code)
            .unwrap();
    }

    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "(a) sync failed: {}", combined(&out));
    let templates = std::fs::read_dir(fresh_dir.join("email-templates"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(
        templates, 5,
        "(a) the sync that created the queue must also pull the five email templates \
         the server provisioned for it:\n{}",
        combined(&out)
    );
    assert_converged(&project, "test", &prefix, "(a) queue created without its templates");

    // (b) Delete the hook the other one runs after; leave the survivor alone.
    for ext in ["json", "py"] {
        let _ = std::fs::remove_file(project.path().join(format!("envs/test/hooks/{validator}.{ext}")));
    }
    let out = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "(b) sync failed: {}", combined(&out));
    let survivor = project.read_json(&format!("envs/test/hooks/{post_validator}.json"));
    assert_eq!(
        survivor["run_after"],
        serde_json::json!([]),
        "(b) the sync that deleted {validator} must also pull the server dropping it \
         from {post_validator}'s run_after"
    );
    assert_converged(&project, "test", &prefix, "(b) hook deleted under a run_after");

    // (c) Delete the fresh queue; its rule stays.
    std::fs::remove_dir_all(&fresh_dir).unwrap();
    let out = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "(c) sync failed: {}", combined(&out));
    let rule = project.read_json(&format!("envs/test/rules/{fresh_rule}.json"));
    assert_eq!(
        rule["queues"],
        serde_json::json!([]),
        "(c) the sync that deleted the queue must also detach it from the rule, \
         which still names it"
    );
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let leftovers: Vec<String> = ["queues", "schemas", "inboxes"]
        .iter()
        .flat_map(|k| lockfile_keys(&lf, k).into_iter().filter(|s| s == &fresh).map(move |s| format!("{k}/{s}")))
        .chain(
            lockfile_keys(&lf, "email_templates")
                .into_iter()
                .filter(|s| s.split('/').nth(1) == Some(fresh.as_str()))
                .map(|s| format!("email_templates/{s}")),
        )
        .collect();
    assert!(
        leftovers.is_empty(),
        "(c) the sync that deleted the queue must also drop what the server refuses to \
         delete apart from it; still in the lockfile: {leftovers:?}\n{}",
        combined(&out)
    );
    let out_text = combined(&out);
    assert!(
        !out_text.contains("could not be deleted"),
        "(c) a queue's own leftovers are not failed deletes:\n{out_text}"
    );
    assert_converged(&project, "test", &prefix, "(c) queue deleted under a rule");

    drop(teardown);
}
