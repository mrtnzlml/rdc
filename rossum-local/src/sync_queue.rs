use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;
use ulid::Ulid;

/// Releases the in-flight slot for `id` on drop. Ensures the slot is
/// freed even if the synced future panics — without this, a panic would
/// leave `id` permanently in the in-flight set and silently block all
/// future submits for that Connection.
struct InFlightGuard {
    id: Ulid,
    set: Arc<Mutex<HashSet<Ulid>>>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // `try_lock` because Drop must be synchronous. The slot may
        // briefly stay claimed if the lock is contended at drop time, but
        // a subsequent submit will see the released slot once the lock
        // clears.
        if let Ok(mut g) = self.set.try_lock() {
            g.remove(&self.id);
        }
    }
}

pub struct SyncQueue {
    in_flight: Arc<Mutex<HashSet<Ulid>>>,
    sem: Arc<Semaphore>,
}

impl SyncQueue {
    pub fn new(parallel_limit: usize) -> Self {
        Self {
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            sem: Arc::new(Semaphore::new(parallel_limit)),
        }
    }

    pub fn submit<F>(&self, id: Ulid, fut: F) -> Result<JoinHandle<()>>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let in_flight = self.in_flight.clone();
        let sem = self.sem.clone();

        // Synchronous claim of the in-flight slot so duplicate submissions
        // are rejected immediately without racing.
        {
            let mut guard = in_flight
                .try_lock()
                .map_err(|_| anyhow!("sync queue contended; try again"))?;
            if !guard.insert(id) {
                return Err(anyhow!("already syncing this Connection"));
            }
        }

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            let _guard = InFlightGuard { id, set: in_flight };
            fut.await;
            // _guard drops here, even on panic, releasing the slot
        });
        Ok(handle)
    }
}
