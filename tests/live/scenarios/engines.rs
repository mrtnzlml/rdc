use crate::support::assert_local::load_lockfile;
use crate::support::assert_remote::assert_remote_field;
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::teardown::Teardown;

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`. `engines_round_trip` never calls `capture_mode` /
/// `load_or_compare` — it has no golden — so, like `fake_sidecars_redaction`,
/// there is nothing here to refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_engines_round_trip() {
    let fake = crate::support::fake::FakeOrg::start().await;
    engines_round_trip(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_engines_round_trip() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    engines_round_trip(&cfg).await;
}

/// Full `engines` / `engine_fields` lifecycle against a real org: pull
/// round-trip, local edit pushed, a field CREATED from a hand-written file, a
/// field DELETED through a tombstone, then the engine itself deleted.
///
/// This scenario deliberately **never binds a queue to the engine**, and that
/// is the whole reason it can run on every live run. A bound engine is refused
/// deletion with `400 engine_attached_to_active_queues` while its queue lives,
/// and then `400 engine_attached_to_queues_waiting_for_deletion` — "after up to
/// 24 hours" — while the queue drains; nulling `queue.engine` to escape is
/// refused too ("Queue does not have an engine"). Unbound, the same objects
/// delete cleanly with `204`. Queue binding is covered by
/// `live_push_create_ordering`, which pays that price once, on purpose.
///
/// Note the field slug: `engine_fields` are keyed in the lockfile by the
/// COMPOUND `<engine-slug>/<field-slug>`, and live on disk under
/// `engines/<engine>/fields/<field>.json`. A field's `name` must match the
/// schema datapoint id it covers, so it is snake_case where its slug is
/// hyphenated — `amount_due` becomes `amount-due.json`.
async fn engines_round_trip(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

    // --- seed: one engine, two fields, no queue anywhere near it ---
    let engine_name = run_id.prefix("engine");
    let (engine_id, engine_url) = client
        .create(
            "engine",
            &serde_json::json!({
                "name": engine_name,
                "type": "extractor",
                "learning_enabled": false,
                "description": "seeded",
            }),
        )
        .await
        .expect("create engine");

    let field_name = format!("{}amount_due", run_id.list_prefix());
    let (field_id, _) = client
        .create(
            "engine_field",
            &serde_json::json!({
                "engine": engine_url,
                "name": field_name,
                "label": "Amount Due",
                "type": "number",
                "subtype": "amount",
            }),
        )
        .await
        .expect("create engine field");

    // --- pull ---
    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(pull.status.success(), "pull failed: {}", combined(&pull));

    let prefix = run_id.list_prefix();
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let engine_slug = lf
        .slug_for_id("engines", engine_id)
        .expect("the seeded engine must be tracked")
        .to_string();
    let field_slug = lf
        .slug_for_id("engine_fields", field_id)
        .expect("the seeded engine field must be tracked")
        .to_string();

    // The compound key is the point: a flat field slug would collide across
    // engines that both define, say, `amount_due`.
    assert_eq!(
        field_slug,
        format!("{engine_slug}/{}amount-due", prefix),
        "engine fields must be keyed by <engine>/<field>"
    );

    let engine_rel = format!("envs/test/engines/{engine_slug}/engine.json");
    let field_rel = format!("envs/test/engines/{engine_slug}/fields/{}amount-due.json", prefix);
    assert!(project.exists(&engine_rel), "missing {engine_rel}");
    assert!(project.exists(&field_rel), "missing {field_rel}");

    let on_disk_field = project.read_json(&field_rel);
    assert_eq!(
        on_disk_field["engine"],
        serde_json::json!(format!("rdc://engines/{engine_slug}")),
        "a field's engine ref must be portable on disk"
    );

    assert_converged(&project, "test", &prefix, "after the initial pull");

    // --- edit both, push, confirm the remote took it ---
    let mut engine = project.read_json(&engine_rel);
    engine["description"] = serde_json::json!("edited by rdc");
    project.write_json(&engine_rel, &engine);

    let mut field = project.read_json(&field_rel);
    field["label"] = serde_json::json!("Amount Due (edited)");
    project.write_json(&field_rel, &field);

    let push = project.run_rdc(&["sync", "test"]);
    assert!(push.status.success(), "push failed: {}", combined(&push));

    assert_remote_field(
        &client,
        "engine",
        engine_id,
        "description",
        &serde_json::json!("edited by rdc"),
    )
    .await
    .expect("the engine edit must reach the remote");
    assert_remote_field(
        &client,
        "engine_field",
        field_id,
        "label",
        &serde_json::json!("Amount Due (edited)"),
    )
    .await
    .expect("the field edit must reach the remote");

    assert_converged(&project, "test", &prefix, "after pushing engine + field edits");

    // --- CREATE a second field from a hand-written file ---
    let new_rel = format!("envs/test/engines/{engine_slug}/fields/{}amount-tax.json", prefix);
    project.write_json(
        &new_rel,
        &serde_json::json!({
            "id": 0,
            "url": "",
            "name": format!("{prefix}amount_tax"),
            "engine": format!("rdc://engines/{engine_slug}"),
            "label": "Amount Tax",
            "type": "number",
            "subtype": "amount",
            "pre_trained_field_id": null,
            "tabular": false,
            "multiline": "false",
        }),
    );

    let create = project.run_rdc(&["sync", "test"]);
    assert!(create.status.success(), "field create failed: {}", combined(&create));

    let lf = load_lockfile(project.path(), "test").expect("lockfile after create");
    let new_id = lf
        .objects
        .get("engine_fields")
        .and_then(|m| m.get(&format!("{engine_slug}/{}amount-tax", prefix)))
        .unwrap_or_else(|| panic!("the created field must be in the lockfile"))
        .id;
    assert!(
        client.find_listed_value("engine_field", new_id).await.expect("list").is_some(),
        "the created field must exist remotely (id {new_id})"
    );

    assert_converged(&project, "test", &prefix, "after creating an engine field");

    // --- DELETE that field through a tombstone ---
    std::fs::remove_file(project.path().join(&new_rel)).expect("removing the field file");
    let del = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(del.status.success(), "field delete failed: {}", combined(&del));
    assert!(
        client.find_listed_value("engine_field", new_id).await.expect("list").is_none(),
        "the tombstoned field must be gone remotely (id {new_id})"
    );

    assert_converged(&project, "test", &prefix, "after deleting an engine field");

    // --- DELETE the engine, with its remaining field ---
    std::fs::remove_dir_all(project.path().join(format!("envs/test/engines/{engine_slug}")))
        .expect("removing the engine dir");
    let del_engine = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(del_engine.status.success(), "engine delete failed: {}", combined(&del_engine));
    assert!(
        client.find_listed_value("engine", engine_id).await.expect("list").is_none(),
        "an UNBOUND engine must delete cleanly (id {engine_id}); if this fails, something \
         bound it to a queue and the 24h rule now applies"
    );

    drop(teardown);
}
