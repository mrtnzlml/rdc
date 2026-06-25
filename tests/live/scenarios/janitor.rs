use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::run_id::RunId;
use crate::support::teardown::teardown_by_prefix;

/// Safety net: delete every `rdc-it-*` object left behind by a crashed run.
/// Deletes ALL harness objects regardless of run-id (the marker prefix).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_janitor_sweep() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let client = LiveClient::connect(&cfg).expect("connect");
    teardown_by_prefix(&client, RunId::marker()).await.expect("janitor sweep");
    // Synchronously-deletable kinds MUST be fully gone after the sweep.
    for kind in ["workspace", "hook", "label", "rule", "inbox"] {
        let left = client.list_ids_by_name_prefix(kind, RunId::marker()).await.unwrap_or_default();
        assert!(left.is_empty(), "janitor left {kind} objects: {left:?}");
    }
    // Queues delete ASYNCHRONOUSLY on this API (DELETE -> 202
    // `deletion_requested`, ~24h purge), and a queue's schema stays
    // 409-referenced until the queue actually purges. So we do NOT require
    // queues/schemas to be empty — the DELETE was accepted; we only report
    // what is still settling. (Schemas have no list endpoint, so they can't
    // be enumerated here anyway.)
    let queues_left = client.list_ids_by_name_prefix("queue", RunId::marker()).await.unwrap_or_default();
    if !queues_left.is_empty() {
        eprintln!(
            "janitor: {} queue(s) still settling (async delete, ~24h purge): {queues_left:?}",
            queues_left.len()
        );
    }
}
