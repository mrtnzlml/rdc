//! What a sync reports beyond its counts: the plan a dry run computed, and
//! the objects a run decided not to write.
//!
//! The plan is assembled in `run_cycle`'s dry-run branch from what it already
//! computes. The skipped items are recorded where each decision is made, into
//! a task-local collector scoped around one cycle (the pattern
//! `stdin_coord::ROUTE` and `journal::item` use), so no driver signature
//! changes. Outside a scope, [`record`] does nothing.

use std::future::Future;
use std::sync::{Arc, Mutex};

/// Which way an item moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanDirection {
    /// To the env.
    Push,
    /// Into the local tree.
    Pull,
    /// Both sides changed: a conflict.
    Both,
}

/// What happens to the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanChange {
    Create,
    Update,
    Delete,
    Conflict,
}

/// The prompt an interactive run raises for the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanPrompt {
    /// The destructive-delete gate (without `interactive`, the run refuses).
    DeleteGate,
    /// A both-changed object.
    Conflict,
    /// A delete on one side and an edit on the other.
    RemoteDelete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub kind: String,
    pub slug: String,
    pub direction: PlanDirection,
    pub change: PlanChange,
    pub prompt: Option<PlanPrompt>,
    /// The log row's note.
    pub note: Option<String>,
}

/// A crashed run's create the dry run's recovery settled: `id` is the object
/// to adopt, `None` when nothing was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRecovery {
    pub kind: String,
    pub slug: String,
    pub id: Option<u64>,
}

/// What a dry run would do, in the log's row order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub items: Vec<PlanItem>,
    pub recovered: Vec<PlanRecovery>,
    /// Every line of the dry run's blocks, as the log prints them.
    pub problems: Vec<String>,
}

/// Why a run did not write an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkipReason {
    /// The delete gate was declined, or not answered.
    DeleteGateDeclined,
    /// A delete whose object changed on the env was put off.
    DeleteDriftSkipped,
    /// A push whose object changed on the env was put off.
    PushDriftSkipped,
    /// A both-changed object was skipped; its remote side is in a shadow.
    ConflictSkipped,
    /// A delete on one side was put off; a marker re-presents it.
    RemoteDeleteSkipped,
    /// A dataset's index drops were declined.
    MdhIndexDropDeclined,
    /// A dataset's row deletes were declined.
    MdhRowDeleteDeclined,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SkippedItem {
    pub kind: String,
    pub slug: String,
    pub reason: SkipReason,
}

type Collector = Arc<Mutex<Vec<SkippedItem>>>;

tokio::task_local! {
    static SKIPPED: Collector;
}

/// Record that the current cycle did not write `kind`/`slug`, and why.
pub fn record(kind: &str, slug: &str, reason: SkipReason) {
    let item = SkippedItem { kind: kind.to_string(), slug: slug.to_string(), reason };
    let _ = SKIPPED.try_with(|c| {
        let mut c = c.lock().unwrap_or_else(|p| p.into_inner());
        if !c.contains(&item) {
            c.push(item);
        }
    });
}

/// Run `f` collecting what it [`record`]s.
pub async fn collect<F: Future>(f: F) -> (F::Output, Vec<SkippedItem>) {
    let c: Collector = Arc::default();
    let out = SKIPPED.scope(c.clone(), f).await;
    let items = std::mem::take(&mut *c.lock().unwrap_or_else(|p| p.into_inner()));
    (out, items)
}

/// The collector this task is scoped to, if any. `rt::spawn` carries it.
pub(crate) fn current() -> Option<Collector> {
    SKIPPED.try_with(Arc::clone).ok()
}

/// Run `f` recording into `c`.
pub(crate) async fn scope<F: Future>(c: Collector, f: F) -> F::Output {
    SKIPPED.scope(c, f).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_once_per_item_and_only_in_scope() {
        record("labels", "a", SkipReason::ConflictSkipped);
        let ((), got) = collect(async {
            record("labels", "a", SkipReason::ConflictSkipped);
            record("labels", "a", SkipReason::ConflictSkipped);
            record("labels", "b", SkipReason::DeleteGateDeclined);
        })
        .await;
        let keys: Vec<(&str, SkipReason)> = got.iter().map(|i| (i.slug.as_str(), i.reason)).collect();
        assert_eq!(keys, vec![("a", SkipReason::ConflictSkipped), ("b", SkipReason::DeleteGateDeclined)]);
    }
}
