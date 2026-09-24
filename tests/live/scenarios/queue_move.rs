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
async fn fake_queue_move() {
    let fake = crate::support::fake::FakeOrg::start().await;
    queue_move(&fake.config()).await;
}

/// The live twin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_queue_move() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    queue_move(&cfg).await;
}

/// Moving a queue to another workspace keeps its email templates.
///
/// A template's lockfile key carries the workspace slug, so a move used to
/// read as delete-plus-create: the sync DELETEd the queue's templates and
/// POSTed them again with new ids, which drops their triggers, and the two
/// unique-typed ones could not be re-created at all. Both directions are
/// covered, each in one sync followed by `assert_converged`:
///
///  (a) the queue's directory is moved locally and `workspace` edited;
///  (b) the queue is moved back on the remote.
///
/// Either way no template may be deleted or created, and every template keeps
/// its id.
async fn queue_move(cfg: &LiveConfig) {
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
    let queue_id = index.id("queue-invoices-main").expect("seeded queue id");
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let q_slug = lf.slug_for_id("queues", queue_id).expect("seeded queue").to_string();
    let ws_slug = |key: &str| {
        lf.slug_for_id("workspaces", index.id(key).expect("seeded workspace id"))
            .expect("seeded workspace in the lockfile")
            .to_string()
    };
    let (main_ws, other_ws) = (ws_slug("ws-main"), ws_slug("ws-secondary"));
    let other_ws_url = client.org_url.replace(
        &format!("organizations/{}", cfg.org_id),
        &format!("workspaces/{}", index.id("ws-secondary").unwrap()),
    );

    // The queue's template ids, keyed by `<queue>/<template>` so a move
    // between workspaces compares equal.
    let template_ids = || -> std::collections::BTreeMap<String, u64> {
        load_lockfile(project.path(), "test")
            .expect("lockfile")
            .objects
            .get("email_templates")
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| k.split('/').nth(1) == Some(q_slug.as_str()))
                    .map(|(k, e)| (k.split_once('/').unwrap().1.to_string(), e.id))
                    .collect()
            })
            .unwrap_or_default()
    };
    let before = template_ids();
    assert_eq!(before.len(), 5, "the seeded queue must own five email templates");
    let no_template_writes = |tr: &crate::support::trace::Trace, ctx: &str| {
        for m in ["POST", "DELETE"] {
            assert!(
                tr.first(m, "email_templates").is_none(),
                "{ctx}: moving a queue must not {m} its email templates"
            );
        }
    };

    // (a) Move the directory locally and point `workspace` at the new one.
    let from = queue_file_path(project.path(), "test", &q_slug, "queue.json")
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(from.starts_with(project.path().join(format!("envs/test/workspaces/{main_ws}"))));
    let to = project.path().join(format!("envs/test/workspaces/{other_ws}/queues/{q_slug}"));
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::rename(&from, &to).unwrap();
    let mut queue: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(to.join("queue.json")).unwrap()).unwrap();
    queue["workspace"] = format!("rdc://workspaces/{other_ws}").into();
    let mut bytes = serde_json::to_vec_pretty(&queue).unwrap();
    bytes.push(b'\n');
    std::fs::write(to.join("queue.json"), bytes).unwrap();

    let (out, tr) = project.run_rdc_traced(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "(a) sync failed: {}", combined(&out));
    no_template_writes(&tr, "(a)");
    assert_eq!(template_ids(), before, "(a) every template must keep its id");
    assert_converged(&project, "test", &prefix, "(a) queue moved locally");

    // (b) Move it back on the remote.
    let main_ws_url = other_ws_url.replace(
        &format!("workspaces/{}", index.id("ws-secondary").unwrap()),
        &format!("workspaces/{}", index.id("ws-main").unwrap()),
    );
    client
        .patch_fields("queue", queue_id, serde_json::json!({ "workspace": main_ws_url }))
        .await
        .expect("move the queue remotely");
    let (out, tr) = project.run_rdc_traced(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "(b) sync failed: {}", combined(&out));
    no_template_writes(&tr, "(b)");
    assert_eq!(template_ids(), before, "(b) every template must keep its id");
    assert!(
        project.exists(&format!("envs/test/workspaces/{main_ws}/queues/{q_slug}/queue.json")),
        "(b) the pull must move the queue's directory back"
    );
    assert!(
        !to.exists(),
        "(b) the queue's old directory must be gone, not left beside the new one"
    );
    assert_converged(&project, "test", &prefix, "(b) queue moved remotely");

    drop(teardown);
}
