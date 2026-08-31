use crate::support::assert_local::load_lockfile;
use crate::support::assert_remote::assert_remote_ref_resolved;
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::snapshot::write_snapshot;
use crate::support::teardown::Teardown;

/// Dependency-ordered CREATE of a whole object graph, against a real org, from
/// a hand-authored snapshot with no lockfile entries.
///
/// Two independent oracles run at once, and they cover different things.
///
/// **The server.** The fixture queue binds the fixture engine, and `POST
/// /queues` validates the queue schema's extracted fields against the bound
/// engine's field NAMES:
///
/// ```text
/// 400 non_field_errors: Engine (id: N) restriction: extracted field
///     'rdc-it-<run>-probe_field' is not present among names of engine fields
/// ```
///
/// So if `engines::push` / `engine_fields::push` ever slide back below
/// `queues::push` — where they were until `0d60d3e`, and where a promote into a
/// fresh env died on its first queue — this scenario fails with that message
/// and nothing else needs to notice.
///
/// **The trace.** `RDC_TRACE_HTTP` records every attempt, so the order is also
/// asserted directly. That matters most for the two edges the server does NOT
/// enforce: `labels → rules` (a rule action's `payload.labels` is resolved at
/// rule-create time, so a late label gives "Invalid hyperlink — No URL match"
/// only for a project that uses label actions) and the deferred relink PATCH
/// landing after both hook POSTs (if it silently stopped firing, the create
/// would still succeed and `run_after` would just be empty).
///
/// # The engine this scenario strands
///
/// Binding is what buys the server-side oracle, and it costs one engine plus
/// one field per run: a bound engine is refused deletion for up to 24 hours
/// after its queue is deleted, with no unbind escape hatch. They carry the run
/// marker, and the janitor collects them on a later run. Do NOT "fix" this by
/// dropping the binding — that would leave only the trace oracle, which mostly
/// restates what the code does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_push_create_ordering() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    write_snapshot(&project, "test", &run_id, &client.org_url);

    // One sync: pushes the whole graph, then pulls the org back.
    let (out, tr) = project.run_rdc_traced(&["sync", "test"]);
    assert!(
        out.status.success(),
        "the fresh-graph sync failed — if stderr carries \"is not present among names of \
         engine fields\", the push order regressed and engine fields are going out after \
         queues again:\n{}",
        combined(&out)
    );

    let prefix = run_id.list_prefix();

    // --- ordering, straight off the wire ---
    tr.assert_before(
        ("POST", "engines"),
        ("POST", "engine_fields"),
        "an engine field's create body carries its engine's URL",
    );
    tr.assert_before(
        ("POST", "engine_fields"),
        ("POST", "queues"),
        "POST /queues validates the schema's extracted fields against the bound engine's \
         field names, so the fields must already exist",
    );
    tr.assert_before(
        ("POST", "workspaces"),
        ("POST", "queues"),
        "the queue create body carries a resolved workspace URL",
    );
    tr.assert_before(
        ("POST", "schemas"),
        ("POST", "queues"),
        "the queue create body carries a resolved schema URL",
    );
    tr.assert_before(
        ("POST", "queues"),
        ("POST", "inboxes"),
        "an inbox belongs to a queue",
    );
    tr.assert_before(
        ("POST", "queues"),
        ("POST", "email_templates"),
        "an email template belongs to a queue",
    );
    tr.assert_before(
        ("POST", "queues"),
        ("POST", "saved_views"),
        "a saved view's queues_filter references a queue",
    );
    tr.assert_before(
        ("POST", "labels"),
        ("POST", "rules"),
        "a rule action's payload.labels is resolved against the lockfile at rule-create \
         time — a late label gives 'Invalid hyperlink - No URL match'",
    );
    tr.assert_before(
        ("POST", "hooks"),
        ("PATCH", "hooks"),
        "run_after is deferred out of the create body and PATCHed by the relink pass once \
         both hooks exist",
    );

    // --- remote truth: the refs actually resolved ---
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let q_slug = format!("{prefix}invoices");
    let queue_id = lf
        .objects
        .get("queues")
        .and_then(|m| m.get(&q_slug))
        .unwrap_or_else(|| panic!("the created queue must be in the lockfile as '{q_slug}'"))
        .id;

    assert_remote_ref_resolved(&client, "queue", queue_id, "schema")
        .await
        .expect("the queue's schema ref must resolve remotely");
    assert_remote_ref_resolved(&client, "queue", queue_id, "engine")
        .await
        .expect("the queue's engine ref must resolve remotely — the binding is the whole point");

    let validator_id = lf
        .objects
        .get("hooks")
        .and_then(|m| m.get(&format!("{prefix}validator")))
        .expect("validator hook in the lockfile")
        .id;
    let post_id = lf
        .objects
        .get("hooks")
        .and_then(|m| m.get(&format!("{prefix}post-validator")))
        .expect("post-validator hook in the lockfile")
        .id;
    let post = client.get_value("hook", post_id).await.expect("GET the post-validator");
    let run_after = post["run_after"].as_array().cloned().unwrap_or_default();
    assert!(
        run_after.iter().any(|u| u.as_str().is_some_and(|s| s.ends_with(&format!("/{validator_id}")))),
        "the deferred relink must have set run_after to the validator's URL; got {run_after:?}"
    );

    let label_id = lf
        .objects
        .get("labels")
        .and_then(|m| m.get(&format!("{prefix}priority")))
        .expect("label in the lockfile")
        .id;
    let rule_id = lf
        .objects
        .get("rules")
        .and_then(|m| m.get(&format!("{prefix}totals")))
        .expect("rule in the lockfile")
        .id;
    let rule = client.get_value("rule", rule_id).await.expect("GET the rule");
    let labels = rule["actions"][0]["payload"]["labels"].as_array().cloned().unwrap_or_default();
    assert!(
        labels.iter().any(|u| u.as_str().is_some_and(|s| s.ends_with(&format!("/{label_id}")))),
        "the rule action's label ref must have resolved to the created label; got {labels:?}"
    );

    // A hand-written snapshot must deploy in ONE cycle — no second pass to
    // settle back-refs the server filled in behind the creates.
    assert_converged(&project, "test", &prefix, "after creating the whole graph in one sync");

    drop(teardown);
}
