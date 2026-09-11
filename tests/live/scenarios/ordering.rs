use crate::support::assert_local::load_lockfile;
use crate::support::assert_remote::assert_remote_ref_resolved;
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::snapshot::write_snapshot;
use crate::support::teardown::Teardown;

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_push_create_ordering() {
    let fake = crate::support::fake::FakeOrg::start().await;
    push_create_ordering(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_push_create_ordering() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    push_create_ordering(&cfg).await;
}

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
async fn push_create_ordering(cfg: &LiveConfig) {
    let run_id = RunId::new();
    let client = LiveClient::connect(cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(cfg).unwrap(), run_id.clone());

    let project = ProjectFixture::init(cfg, &["test"]).expect("init");
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

    // A hand-written snapshot must deploy in ONE cycle — GIVEN that every
    // object the server auto-provisions behind a create is itself declared
    // in the snapshot. Creating this queue makes Rossum synchronously spawn
    // five system-managed default email templates (see the fixture files
    // under `email-templates/` named `annotation-status-change-*`,
    // `default-rejection-template` and `email-with-no-processable-attachments`
    // — LOAD-BEARING, not sample noise: see the comment in
    // `support/snapshot.rs` next to the fixture-file-count test). Because
    // `src/cli/sync/mod.rs` lists the remote catalog ONCE, before pushing,
    // rdc never discovers those five on its own within this cycle — it only
    // settles them because `push/email_templates.rs`'s adopt-or-create path
    // (`pick_adoption_id`) re-lists email templates mid-push, AFTER the
    // queue already exists, and finds a local file to match each one by
    // name (the three `custom`-typed ones) or by `type` (the other two) and
    // PATCHes our content into them then and there. Omit any one of the
    // five from the fixture and this assertion still fails — correctly:
    // that object would then take a genuine second cycle to surface, which
    // is real rdc behaviour, not a test bug.
    assert_converged(&project, "test", &prefix, "after creating the whole graph in one sync");

    // -------------------------------------------------------------------------
    // Deletes: the cascade order, and skip-and-continue against a REAL refusal.
    // -------------------------------------------------------------------------
    //
    // Tombstone THIS RUN'S objects — and only this run's.
    //
    // Every path below is prefix-scoped, and that is not tidiness: the `sync`
    // above pulled the WHOLE sandbox org into this tree (a couple of hundred
    // objects, including real workspaces, hooks, rules and the org's four real
    // engines). Removing `envs/test/hooks` wholesale would tombstone all of
    // them, and the `--allow-deletes` below would then delete real content off
    // a shared org. Never widen these paths.
    //
    // The engine is deliberately left IN the tombstone set even though the
    // server will refuse it: that refusal is the point of the second assertion
    // below.
    for dir in [
        format!("envs/test/workspaces/{prefix}ws"),
        format!("envs/test/engines/{prefix}engine"),
    ] {
        std::fs::remove_dir_all(project.path().join(&dir))
            .unwrap_or_else(|e| panic!("removing {dir}: {e}"));
    }
    for file in [
        format!("envs/test/hooks/{prefix}validator.json"),
        format!("envs/test/hooks/{prefix}validator.py"),
        format!("envs/test/hooks/{prefix}post-validator.json"),
        format!("envs/test/hooks/{prefix}post-validator.py"),
        format!("envs/test/rules/{prefix}totals.json"),
        format!("envs/test/labels/{prefix}priority.json"),
        format!("envs/test/saved-views/{prefix}view.json"),
    ] {
        std::fs::remove_file(project.path().join(&file))
            .unwrap_or_else(|e| panic!("removing {file}: {e}"));
    }

    // Belt and braces: nothing outside this run may have been tombstoned. A
    // widened path above would show up here as a lockfile entry with no file,
    // BEFORE `--allow-deletes` turns it into a DELETE.
    //
    // The check is a substring test against the env tree ONLY
    // (`env_files_matching`, not `TreeSnapshot::capture`): a lockfile entry
    // with no matching file under `envs/test` means its file is gone — i.e.
    // it has become a tombstone. `TreeSnapshot::capture` cannot be used here:
    // it also walks the base cache (`.rdc/state/test.base`, which the
    // tombstone loop above never touches, so a "file" would still be found
    // there for anything just deleted from the env tree) and then adds a
    // synthetic `lockfile:<kind>/<slug>` entry for every matching lockfile
    // slug — the very slug this loop is asking about — so the question "does
    // this lockfile slug still have a file?" would be answered by the
    // question itself and could never come back empty.
    //
    // Only kinds whose slug appears VERBATIM in their on-disk path can be
    // checked this way. Two are skipped because their slugs are compound and
    // the path interleaves extra segments, so under this real (non-vacuous)
    // substring test they would now false-FAIL every one of them as missing:
    //
    //   email_templates  slug `<ws>/<queue>/<tpl>`
    //                    path `workspaces/<ws>/queues/<queue>/email-templates/<tpl>.json`
    //   engine_fields    slug `<engine>/<field>`
    //                    path `engines/<engine>/fields/<field>.json`
    //
    // Skipping them costs nothing: both live UNDER a parent this loop does
    // check (a workspace, an engine), so the realistic widening — removing a
    // whole top-level directory — is still caught via the parent.
    // `organization`, `mdh_*` and `workflow_*` are skipped for the same reason.
    let lf_before_del = load_lockfile(project.path(), "test").expect("lockfile before deletes");
    for (kind, entries) in &lf_before_del.objects {
        if matches!(kind.as_str(), "email_templates" | "engine_fields" | "organization")
            || kind.starts_with("mdh")
            || kind.starts_with("workflow")
        {
            continue;
        }
        for slug in entries.keys() {
            if slug.starts_with("rdc-it-") {
                continue;
            }
            let matches =
                crate::support::converge::env_files_matching(project.path(), "test", slug);
            assert!(
                matches > 0,
                "about to delete something this run does not own: {kind}/{slug} has a lockfile entry but no file on disk — a tombstone path was widened"
            );
        }
    }

    let (del, dtr) = project.run_rdc_traced(&["sync", "test", "--allow-deletes"]);
    assert!(del.status.success(), "the delete pass failed: {}", combined(&del));

    // Children before parents. `saved_views` is NOT asserted: nothing
    // references a saved view, so `push::deletes` documents its position among
    // the leaves as free, and pinning it would freeze an arbitrary choice.
    for child in ["rules", "hooks", "email_templates", "inboxes"] {
        dtr.assert_before(
            ("DELETE", child),
            ("DELETE", "queues"),
            "a queue's children must be deleted before the queue",
        );
    }
    dtr.assert_before(
        ("DELETE", "queues"),
        ("DELETE", "schemas"),
        "a schema cannot be deleted while a queue references it (409 conflict_referenced)",
    );
    dtr.assert_before(
        ("DELETE", "schemas"),
        ("DELETE", "workspaces"),
        "children before parents",
    );

    // Skip-and-continue, against a refusal that is REAL and TEMPORARY.
    //
    // `run_deletes` is documented as never propagating a per-object DELETE
    // failure: it warns, tallies `DeleteCounts::failed`, LEAVES THE LOCKFILE
    // ENTRY so a later sync retries, and keeps going so every sibling and
    // parent still gets deleted. The suite's only other coverage of that
    // contract is a unique-typed email template, which is refused PERMANENTLY;
    // a bound engine is refused only until its queue finishes purging, which is
    // the case that actually needs the lockfile entry kept.
    //
    // This is not a defect pin. rdc's cascade puts engines before queues, which
    // looks wrong, but no ordering could help: the server's rule is "after the
    // queue is deleted, up to 24 hours", and `DELETE /queues` only returns `202
    // deletion_requested`.
    let engine_slug = format!("{prefix}engine");
    let stderr = combined(&del);
    // Match the exact warning `push::deletes::run_deletes` emits for the
    // engine itself (`"{kind}/{slug} delete failed (skipped): {e:#}"` with
    // `kind == "engines"`). A plain `stderr.contains(&engine_slug)` is also
    // satisfied by the engine FIELD's own refusal warning, because the
    // field's slug (`<engine_slug>/<field_slug>`) contains the engine's slug
    // as a substring.
    let expected_warning = format!("engines/{engine_slug} delete failed (skipped)");
    assert!(
        stderr.contains(&expected_warning),
        "the refused engine delete must be warned about by slug, not swallowed:\n{stderr}"
    );

    let lf_after = load_lockfile(project.path(), "test").expect("lockfile after deletes");
    assert!(
        lf_after.objects.get("engines").is_some_and(|m| m.contains_key(&engine_slug)),
        "a refused delete must KEEP its lockfile entry so a later sync retries it"
    );

    // Everything that could go, went.
    assert!(
        client.find_listed_value("queue", queue_id).await.expect("list queues").is_none()
            || client
                .get_value("queue", queue_id)
                .await
                .map(|v| v["status"] == "deletion_requested")
                .unwrap_or(false),
        "the queue must be deleted or draining"
    );
    for (kind, id) in [("hook", validator_id), ("hook", post_id), ("rule", rule_id), ("label", label_id)] {
        assert!(
            client.find_listed_value(kind, id).await.expect("list").is_none(),
            "{kind} {id} must be gone after the delete pass"
        );
    }

    drop(teardown);
}
