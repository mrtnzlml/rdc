use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;
use ulid::Ulid;

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
            fut.await;
            in_flight.lock().await.remove(&id);
        });
        Ok(handle)
    }
}
