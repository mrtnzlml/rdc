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

    // Schemas: delete by derived id, now that their queues are gone (avoids 409).
    for id in schema_ids {
        if let Err(e) = client.delete("schema", id).await {
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
    Ok(())
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
