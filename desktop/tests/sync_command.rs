// The Tauri command itself requires a full Tauri AppHandle, which is
// awkward to construct in a unit test. We test the underlying
// `run_sync` already (Task 11). Here we just assert that the queue
// rejects double-submissions for the same Connection.

use rossum_local::sync_queue::SyncQueue;
use std::time::Duration;
use ulid::Ulid;

#[tokio::test]
async fn double_submit_rejected() {
    let q = SyncQueue::new(2);
    let id = Ulid::new();
    let _h = q
        .submit(id, async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .unwrap();
    let err = q.submit(id, async move {}).err().unwrap();
    assert!(format!("{err}").contains("already syncing"));
}
