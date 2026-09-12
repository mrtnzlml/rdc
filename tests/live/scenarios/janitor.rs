use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::run_id::RunId;
use crate::support::teardown::teardown_by_prefix;

/// The fake-backed twin. Runs in a plain `cargo test`; see
/// `crate::support::fake`.
///
/// Against a FRESH fake org (what `FakeOrg::start()` always hands back) this
/// asserts nothing that can fail: every kind list here is checked via
/// `list_ids_by_name_prefix(...).unwrap_or_default()` (an error and "no
/// objects" are indistinguishable to the assertion), and a fresh org already
/// has zero objects of every kind before the sweep even runs — so every
/// `left.is_empty()` / `queues_left.is_empty()` check is vacuously true
/// regardless of whether `teardown_by_prefix` does anything at all. The MDH
/// half is even more vacuous: the fake has no Data Storage route at all
/// (`mod.rs::route`'s doc comment: "Everything outside the API prefix — Data
/// Storage included — is a flat 404"), `drop_mdh_collections_by_prefix`
/// swallows that failure and returns `Ok(())` unconditionally, and the
/// trailing `list_collection_names().await.unwrap_or_default()` again turns
/// the resulting error into an empty vec. So this port is NOT falsifiable
/// against a fresh fake org — it cannot go red no matter what
/// `teardown_by_prefix` or the MDH sweep actually do. See the task report for
/// the fuller reasoning; it is reported as an extraction that runs green by
/// construction, not as a scenario this port meaningfully protects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_janitor_sweep() {
    let fake = crate::support::fake::FakeOrg::start().await;
    janitor_sweep(&fake.config()).await;
}

/// The live twin. Unchanged: same `#[ignore]`, same env gate, so
/// `cargo test --test live -- --ignored` still selects exactly the live set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_janitor_sweep() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    janitor_sweep(&cfg).await;
}

/// Safety net: delete every `rdc-it-*` object left behind by a crashed run.
/// Deletes ALL harness objects regardless of run-id (the marker prefix).
async fn janitor_sweep(cfg: &LiveConfig) {
    let client = LiveClient::connect(cfg).expect("connect");
    teardown_by_prefix(&client, RunId::marker()).await.expect("janitor sweep");

    // Sweep the TARGET org too when one is configured: the promotion scenarios
    // create objects there, and a crashed run leaves them behind exactly the
    // same way. Its assertions are the source org's, below — this sweep is
    // best-effort, because a janitor that hard-failed on the second org would
    // stop it cleaning the first.
    if let Some(tgt) = cfg.target.clone() {
        let tgt_client = LiveClient::connect_creds(&tgt).expect("connect (target)");
        teardown_by_prefix(&tgt_client, RunId::marker())
            .await
            .expect("janitor sweep (target org)");
        for kind in ["workspace", "hook", "label", "rule", "inbox", "email_template"] {
            let left = tgt_client
                .list_ids_by_name_prefix(kind, RunId::marker())
                .await
                .unwrap_or_default();
            assert!(left.is_empty(), "janitor left {kind} objects in the target org: {left:?}");
        }
    }
    // Synchronously-deletable kinds MUST be fully gone after the sweep.
    for kind in ["workspace", "hook", "label", "rule", "inbox", "saved_view"] {
        let left = client.list_ids_by_name_prefix(kind, RunId::marker()).await.unwrap_or_default();
        assert!(left.is_empty(), "janitor left {kind} objects: {left:?}");
    }

    // Engines (and their fields) are deliberately NOT asserted empty. An
    // engine that was bound to a queue is undeletable until that queue
    // finishes purging — up to 24 hours — so a sweep run soon after
    // `live_push_create_ordering` legitimately leaves one behind, and it goes
    // on the next run. A field can strand for even longer: teardown's field
    // sweep 409s while its schema still exists, and that schema can become
    // uncollectible once its queue soft-deletes. Report the backlog instead
    // of asserting on it, so a number that keeps climbing is visible rather
    // than silent — for either kind.
    for kind in ["engine", "engine_field"] {
        let left = client.list_ids_by_name_prefix(kind, RunId::marker()).await.unwrap_or_default();
        if !left.is_empty() {
            eprintln!(
                "janitor: {} harness {kind}(s) still pending cleanup (expected; they go on \
                 a later run): {:?}",
                left.len(),
                left
            );
        }
    }
    // Soft-deleted queues (status `deletion_requested` / workspace null) are
    // treated as deleted and excluded from the listing, so the sweep must leave
    // no live queues behind either.
    let queues_left = client.list_ids_by_name_prefix("queue", RunId::marker()).await.unwrap_or_default();
    assert!(queues_left.is_empty(), "janitor left live queue objects: {queues_left:?}");

    // MDH: drop every throwaway `rdc_it_*` collection a crashed run left behind.
    crate::support::teardown::drop_mdh_collections_by_prefix(
        cfg,
        crate::support::mdh::MDH_COLLECTION_MARKER,
    )
    .await
    .expect("janitor mdh sweep");
    // Collection drop is async (202); poll until none remain (bounded).
    let raw = crate::support::mdh::MdhRaw::connect(cfg).expect("connect mdh");
    let mut remaining = raw.list_collection_names().await.unwrap_or_default();
    let mut waited = 0;
    while remaining
        .iter()
        .any(|n| n.starts_with(crate::support::mdh::MDH_COLLECTION_MARKER))
        && waited < 30
    {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        remaining = raw.list_collection_names().await.unwrap_or_default();
        waited += 1;
    }
    let leftover: Vec<_> = remaining
        .into_iter()
        .filter(|n| n.starts_with(crate::support::mdh::MDH_COLLECTION_MARKER))
        .collect();
    assert!(leftover.is_empty(), "janitor left MDH collections: {leftover:?}");
}
