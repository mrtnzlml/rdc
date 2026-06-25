use crate::support::client::LiveClient;
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

/// RAII guard: on drop, deletes everything the run created. Holds its own
/// Tokio runtime handle so it can clean up even from a panicking test.
#[allow(dead_code)]
pub struct Teardown {
    client: LiveClient,
    run_id: RunId,
}

#[allow(dead_code)]
impl Teardown {
    pub fn new(client: LiveClient, run_id: RunId) -> Teardown {
        Teardown { client, run_id }
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
        // Build a short-lived runtime to run async deletes from Drop.
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("teardown: could not build runtime: {e}");
                return;
            }
        };
        if let Err(e) = rt.block_on(teardown_by_prefix(&self.client, &prefix)) {
            eprintln!("teardown: {e:#}");
        }
    }
}
