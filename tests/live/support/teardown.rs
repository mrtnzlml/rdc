use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::run_id::RunId;
use anyhow::Result;

/// Delete every object whose name starts with `prefix`, in dependency order:
/// children before parents. Schemas have NO list endpoint and rdc's delete
/// order is `queues -> schemas`, so schema ids are derived from the `schema`
/// URL of the prefix-matched queues (captured BEFORE the queues are deleted)
/// and deleted right after the queues. Tolerant: a not-found / already-deleting
/// object is logged, not fatal.
#[allow(dead_code)]
pub async fn teardown_by_prefix(client: &LiveClient, prefix: &str) -> Result<()> {
    // Capture schema ids BEFORE deleting queues (schemas can't be listed).
    let schema_ids = match client.schema_ids_for_queue_prefix(prefix).await {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("teardown: could not collect schema ids (continuing): {e:#}");
            Vec::new()
        }
    };

    // Listable child kinds, in order, down to queues.
    for kind in ["email_template", "rule", "hook", "inbox", "queue"] {
        let found = match client.list_ids_by_name_prefix(kind, prefix).await {
            Ok(v) => v,
            Err(_) => continue, // kind not listable in isolation
        };
        for (id, name) in found {
            if let Err(e) = client.delete(kind, id).await {
                eprintln!("teardown: delete {kind} {id} ({name}) failed (continuing): {e:#}");
            }
        }
    }

    // Schemas: delete by derived id, now that their queues are gone.
    //
    // A queue DELETE is ASYNC — it returns 202 `deletion_requested` and the
    // queue lingers for a while — so a schema delete issued immediately after
    // races it and gets `409 Cannot delete schema because it is referenced
    // from queue '<id>'`. Observed on the live sandbox on the very first run of
    // the expanded suite, and the cost is real: an undeleted schema cannot be
    // listed (there is no schema list endpoint), so nothing — not even the
    // janitor — can ever find it again. Retry with a short backoff instead.
    for id in schema_ids {
        if let Err(e) = delete_schema_with_retry(client, id).await {
            eprintln!("teardown: delete schema {id} failed (continuing): {e:#}");
        }
    }

    // Parents last.
    for kind in ["workspace", "label"] {
        let found = match client.list_ids_by_name_prefix(kind, prefix).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        for (id, name) in found {
            if let Err(e) = client.delete(kind, id).await {
                eprintln!("teardown: delete {kind} {id} ({name}) failed (continuing): {e:#}");
            }
        }
    }

    // Saved views: flat, org-scoped, and referenced by nothing else this
    // sweep deletes, so their position is free (same reasoning as
    // `push::deletes`' cascade-order comment). Listed WITHOUT the
    // shared-only filter `rdc` itself applies, so a private view a scenario
    // created alongside a shared one — never reachable through the
    // lockfile-driven cleanup any other kind gets — is still swept here.
    if let Ok(found) = client.list_ids_by_name_prefix("saved_view", prefix).await {
        for (id, name) in found {
            if let Err(e) = client.delete("saved_view", id).await {
                eprintln!("teardown: delete saved_view {id} ({name}) failed (continuing): {e:#}");
            }
        }
    }
    Ok(())
}

/// Delete a schema, retrying while the queue that references it is still
/// finishing its asynchronous delete. Gives up after ~15s.
#[allow(dead_code)]
async fn delete_schema_with_retry(client: &LiveClient, id: u64) -> Result<()> {
    const ATTEMPTS: usize = 10;
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..ATTEMPTS {
        match client.delete("schema", id).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Only a still-referenced schema is worth waiting out; anything
                // else (404 already gone, 403) will not improve with time.
                if !format!("{e:#}").contains("conflict_referenced") {
                    return Err(e);
                }
                last = Some(e);
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("schema {id} still referenced after retries")))
}

/// Drop every MDH collection whose name starts with `marker` (the throwaway
/// `rdc_it_*` collections this harness creates). Best-effort; async 202 drops.
#[allow(dead_code)]
pub async fn drop_mdh_collections_by_prefix(cfg: &LiveConfig, marker: &str) -> anyhow::Result<()> {
    let raw = crate::support::mdh::MdhRaw::connect(cfg)?;
    let names = match raw.list_collection_names().await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("teardown(mdh): list collections failed (continuing): {e:#}");
            return Ok(());
        }
    };
    for name in names.into_iter().filter(|n| n.starts_with(marker)) {
        if let Err(e) = raw.drop_collection(&name).await {
            eprintln!("teardown(mdh): drop collection {name} failed (continuing): {e:#}");
        }
    }
    Ok(())
}

/// RAII guard: on drop, deletes everything the run created. Holds its own
/// Tokio runtime handle so it can clean up even from a panicking test.
#[allow(dead_code)]
pub struct Teardown {
    client: LiveClient,
    run_id: RunId,
    cfg: Option<LiveConfig>,
}

#[allow(dead_code)]
impl Teardown {
    pub fn new(client: LiveClient, run_id: RunId) -> Teardown {
        Teardown { client, run_id, cfg: None }
    }
    /// Like `new`, but also drops this run's throwaway MDH collection(s) on drop.
    pub fn with_mdh(client: LiveClient, run_id: RunId, cfg: LiveConfig) -> Teardown {
        Teardown { client, run_id, cfg: Some(cfg) }
    }
    pub fn client(&self) -> &LiveClient {
        &self.client
    }
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        let prefix = self.run_id.list_prefix();
        let client = &self.client;
        let run_id = &self.run_id;
        let cfg = self.cfg.as_ref();
        // `Drop` fires INSIDE the test's tokio runtime (the scenarios are
        // `#[tokio::test]`). Calling `block_on` on the current thread there
        // panics ("Cannot start a runtime from within a runtime") — and if
        // the test is already unwinding from a failed assertion, that second
        // panic aborts the process (SIGABRT). Run the async teardown on a
        // dedicated OS thread instead: it has no ambient runtime, so
        // `block_on` is legal there. `thread::scope` joins before `drop`
        // returns, so borrowing `client`/`prefix` is sound.
        std::thread::scope(|s| {
            s.spawn(|| {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("teardown: could not build runtime: {e}");
                        return;
                    }
                };
                if let Err(e) = rt.block_on(teardown_by_prefix(client, &prefix)) {
                    eprintln!("teardown: {e:#}");
                }
                if let Some(cfg) = cfg {
                    // Drop only THIS run's throwaway collection.
                    let coll = crate::support::mdh::mdh_collection_name(run_id);
                    if let Ok(raw) = crate::support::mdh::MdhRaw::connect(cfg)
                        && let Err(e) = rt.block_on(raw.drop_collection(&coll))
                    {
                        eprintln!("teardown(mdh): drop {coll} failed (continuing): {e:#}");
                    }
                }
            });
        });
    }
}
