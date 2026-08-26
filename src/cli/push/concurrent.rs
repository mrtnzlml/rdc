//! The push path's concurrency primitive.
//!
//! Every update-capable push driver has the same shape: per changed slug, do
//! network work that needs only `&Lockfile`, then apply the result to the
//! working tree and the lockfile, which needs `&mut Lockfile`. The network
//! half is the slow half and its items are independent; the apply half must
//! stay sequential, in slug order, and owns every interactive prompt.
//!
//! This module owns that split and nothing else — no driver logic lives here.

use anyhow::Result;
use futures::stream::StreamExt;
use std::future::Future;

/// Bound on how many PATCHes are outstanding at once.
///
/// This is NOT the throughput control. The core API's token bucket
/// ([`crate::api::rate_limit::RateLimiter::rossum_core_api`], 10 req/s) is. At
/// the measured PATCH latencies — 133-140ms for a rule, 396ms for a hook — a
/// fan-out of 5 attempts 12-37 req/s, which the bucket then meters down to 10.
/// Raising this constant buys nothing and only widens the blast radius of a
/// failure mid-batch.
pub const PUSH_FANOUT: usize = 5;

/// What one item's network stage decided. `T` is the driver's own type: the
/// server response, or a small struct when the driver must carry extra state
/// (deferred refs, a secrets hash) across to its apply stage.
pub enum Prepared<T> {
    /// The PATCH went through; `updated` is what the apply stage writes back.
    Patched { slug: String, updated: T },
    /// The remote drifted from the recorded base, so this item was NOT
    /// patched. The driver re-runs it on the sequential path, where
    /// `resolve_push_drift` can prompt without ever interleaving with another
    /// item's prompt (`stdin_coord` stays the single stdin owner).
    NeedsPrompt { slug: String },
    /// Nothing to send. `event` is the exact progress line to emit on the
    /// sequential path, so skip wording and skip ordering are both preserved.
    Skipped { slug: String, event: String },
}

impl<T> Prepared<T> {
    pub fn slug(&self) -> &str {
        match self {
            Prepared::Patched { slug, .. }
            | Prepared::NeedsPrompt { slug }
            | Prepared::Skipped { slug, .. } => slug,
        }
    }
}

/// Run `prepare` over `items` with bounded fan-out, returning one result per
/// item **in input order**.
///
/// `buffered`, not `buffer_unordered`: concurrency is identical, but results
/// are delivered in the order the items went in, so the caller's sequential
/// apply stage — and therefore every progress line, disk write and lockfile
/// write — keeps the driver's existing slug order. Nothing in the push
/// transcript reorders.
///
/// Every item is polled to completion even after one fails, so a mid-batch
/// error never strands a PATCH the server already applied: the caller applies
/// every `Ok` and then propagates the first `Err`. That makes the
/// inconsistency window on failure SMALLER than the sequential loop's, which
/// aborted with the failing item's effects unrecorded.
pub async fn prepare_all<I, T, F, Fut>(items: I, prepare: F) -> Vec<Result<Prepared<T>>>
where
    I: IntoIterator,
    F: Fn(I::Item) -> Fut,
    Fut: Future<Output = Result<Prepared<T>>>,
{
    futures::stream::iter(items)
        .map(prepare)
        .buffered(PUSH_FANOUT)
        .collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `buffered`, not `buffer_unordered`: results must come back in INPUT
    /// order so the sequential apply stage keeps the driver's slug order. The
    /// delays here make completion order the exact reverse of input order.
    #[tokio::test(flavor = "multi_thread")]
    async fn prepare_all_preserves_input_order() {
        let out = prepare_all(0..6usize, |i| async move {
            tokio::time::sleep(std::time::Duration::from_millis(
                (6 - i as u64) * 20,
            ))
            .await;
            Ok(Prepared::<usize>::Patched { slug: format!("s{i}"), updated: i })
        })
        .await;
        let slugs: Vec<String> = out
            .into_iter()
            .map(|r| r.unwrap().slug().to_string())
            .collect();
        assert_eq!(slugs, vec!["s0", "s1", "s2", "s3", "s4", "s5"]);
    }

    /// Spec D10: a mid-batch failure must not strand PATCHes the server
    /// already applied. Every item is polled to completion and returned, so
    /// the caller can apply every success before propagating the first error.
    #[tokio::test(flavor = "multi_thread")]
    async fn prepare_all_returns_every_item_even_after_a_failure() {
        let out = prepare_all(0..5usize, |i| async move {
            if i == 2 {
                anyhow::bail!("boom");
            }
            Ok(Prepared::<usize>::Patched { slug: format!("s{i}"), updated: i })
        })
        .await;
        assert_eq!(out.len(), 5, "every item must be represented");
        assert!(out[2].is_err(), "the failing item must report its error");
        assert_eq!(
            out.iter().filter(|r| r.is_ok()).count(),
            4,
            "the other four must have completed, not been cancelled"
        );
    }

    /// The fan-out is a bound on outstanding requests, so it must actually
    /// bound them — the token bucket is the throughput control underneath.
    #[tokio::test(flavor = "multi_thread")]
    async fn prepare_all_never_exceeds_push_fanout() {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let out = prepare_all(0..40usize, |i| {
            let live = live.clone();
            let peak = peak.clone();
            async move {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                live.fetch_sub(1, Ordering::SeqCst);
                Ok(Prepared::<usize>::Patched { slug: format!("s{i}"), updated: i })
            }
        })
        .await;
        assert_eq!(out.len(), 40);
        assert!(
            peak.load(Ordering::SeqCst) >= 2,
            "the primitive must actually overlap work — `buffered(1)` would \
             satisfy every other assertion in this module",
        );
        assert!(
            peak.load(Ordering::SeqCst) <= PUSH_FANOUT,
            "at most PUSH_FANOUT may be in flight, saw {}",
            peak.load(Ordering::SeqCst),
        );
    }
}
