use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::run_id::RunId;
use anyhow::Result;

/// Delete every object whose name starts with `prefix`, in dependency order:
/// children before parents.
///
/// Schemas are swept twice over, and both halves are needed. rdc's delete
/// order is `queues -> schemas`, so a schema id is derived from the `schema`
/// URL of the prefix-matched queues, captured BEFORE those queues are
/// deleted: that is the precise path, and it finds a schema even if its name
/// never carried the run prefix. What that path cannot delete today — because
/// its queue is still inside its 24-hour purge window — is collected on a
/// LATER run by the orphan sweep, which lists `/schemas` by name prefix. The
/// orphan half was missing for months on a false premise; see the schema
/// block below.
///
/// Tolerant throughout: a not-found / already-deleting object is logged, not
/// fatal.
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
    // A queue DELETE is ASYNC and the queue then lingers for **24 hours** —
    // the server stamps `delete_after = <delete time> + 24h`, measured across
    // 29 queues at 24.01–24.04h. Its schema stays referenced that entire
    // window, so this DELETE answers `409 Cannot delete schema because it is
    // referenced from queue '<id>'` and NO retry budget can outlast it.
    //
    // An earlier version slept 10 × 1.5s here trying to ride that out. It
    // could never have worked, and it cost ~15s per schema: one live pair of
    // runs spent roughly 7 minutes asleep in this loop. Try once, log, move
    // on — and let the orphan sweep below (or a later run's janitor) collect
    // it, exactly as engines are already handled further down.
    //
    // See README.md's live-testing section for the user-facing statement of
    // this; keep that the authoritative copy rather than restating it here.
    let mut attempted: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for id in schema_ids {
        attempted.insert(id);
        if let Err(e) = client.delete("schema", id).await {
            eprintln!("teardown: schema {id} still held, deferred to the janitor ({e:#})");
        }
    }

    // Orphan schemas left by EARLIER runs. Once a queue finishes its 24h
    // purge its schema becomes deletable again — but nothing was ever coming
    // back for it, so they accumulated without bound: the sandbox org held
    // 385 of them, 90% of every schema in it.
    //
    // What made that permanent was a false belief, recorded in this very
    // comment for months, that a schema which misses its teardown window
    // "cannot be listed (there is no schema list endpoint), so nothing — not
    // even the janitor — can ever find it again". The endpoint exists:
    // `GET /schemas?page_size=100&page=N` returns every schema with its
    // `name`. rdc's own `pull::common` comment says as much ("the `/schemas`
    // list omits `content`, so the body must be fetched by id"). The gap was
    // in `RossumClient`, which has no `list_schemas` because rdc never needs
    // one — not in the API.
    //
    // Swept during ordinary teardown, not only in the janitor, so a normal
    // run cleans up after its predecessors. Position matters: it must precede
    // the engine_field sweep below, whose `409 conflict_referenced` clears
    // once the schema covering the field's name is gone.
    match client.list_ids_by_name_prefix("schema", prefix).await {
        Ok(found) => {
            for (id, name) in found {
                if attempted.contains(&id) {
                    continue; // just tried above; its queue is still draining
                }
                if let Err(e) = client.delete("schema", id).await {
                    eprintln!("teardown: orphan schema {id} ({name}) still held ({e:#})");
                }
            }
        }
        Err(e) => eprintln!("teardown: listing orphan schemas failed (continuing): {e:#}"),
    }

    // Engine fields, then engines — after queues and schemas, and the two
    // halves are here for different reasons.
    //
    // The FIELD sweep genuinely benefits from the position: `DELETE
    // /engine_fields/<id>` answers `409 conflict_referenced` ("Cannot delete
    // engine field used in a schema") while the schema that its name covers is
    // still around, and that clears once the schema above is gone.
    //
    // The ENGINE sweep cannot be helped by any ordering. An engine that was
    // ever bound to a queue is refused with `400
    // engine_attached_to_active_queues` while the queue lives, and then with
    // `400 engine_attached_to_queues_waiting_for_deletion` — "after up to 24
    // hours" — for as long as the queue is draining. `DELETE /queues` returns
    // `202 deletion_requested`, so the queue is never actually gone by the time
    // this runs. Best-effort on purpose: log and leave it, and a later run's
    // janitor collects it. There is no retry loop because the window is a day,
    // not the fifteen seconds `delete_schema_with_retry` waits out.
    for kind in ["engine_field", "engine"] {
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
