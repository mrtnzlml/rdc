use rossum_local::sync_queue::SyncQueue;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ulid::Ulid;

#[tokio::test]
async fn second_sync_for_same_connection_is_rejected_while_first_runs() {
    let q = SyncQueue::new(4);
    let id = Ulid::new();
    let started = Arc::new(AtomicUsize::new(0));

    let s2 = started.clone();
    let _h1 = q
        .submit(id, async move {
            s2.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(10)).await;
    let err = q
        .submit(id, async move {})
        .err()
        .expect("second submit rejected");
    assert!(format!("{err:#}").contains("already syncing"));
}

#[tokio::test]
async fn distinct_connections_run_in_parallel_up_to_limit() {
    let q = SyncQueue::new(2);
    let counter = Arc::new(AtomicUsize::new(0));

    let ids: Vec<_> = (0..2).map(|_| Ulid::new()).collect();
    let mut handles = Vec::new();
    for id in &ids {
        let c = counter.clone();
        handles.push(
            q.submit(*id, async move {
                c.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
            })
            .unwrap(),
        );
    }
    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn panic_in_fut_releases_in_flight_slot() {
    let q = SyncQueue::new(4);
    let id = Ulid::new();

    let h = q
        .submit(id, async move {
            panic!("deliberate test panic");
        })
        .unwrap();

    // Wait for the panicked task to finish.
    let _ = h.await; // JoinError expected; ignore.
    // Give Drop a moment to run + try_lock to release the slot.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // A second submit for the same id MUST succeed (slot released).
    let _h2 = q
        .submit(id, async move {})
        .expect("slot should be released after panic");
}
