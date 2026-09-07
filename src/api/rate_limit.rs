//! Client-side token-bucket rate limiter for the two services rdc paces:
//! [`crate::api::RossumClient`] (the core API, via [`RateLimiter::rossum_core_api`])
//! and [`crate::api::data_storage::DataStorageClient`] (Data Storage / MDH,
//! via [`RateLimiter::rossum_data_storage`]). They throttle independently on
//! the same token (spec S5), so each gets its own bucket rather than sharing
//! one — a shared bucket would spend core tokens on Data Storage calls
//! nobody asked us to pace.
//!
//! Rossum's ingress rate limiter enforces `default.core_api` at
//! **10 req/s with burst 10** (window 1 s). Empirically verified against
//! `api.elis.rossum.ai/v1` on 2026-05-22:
//!
//! - The `x-limiter-core-api` header on 200 responses reported
//!   `{"config":{"rate_limit":10,"burst":10,"window":1,"action":"enforce"}}`
//!   **at the time**. This is now HISTORICAL, not a live mechanism: the
//!   2026-08-24 Data Storage probes (spec S7) verified the header is no
//!   longer present on responses from either service. Do not read a policy
//!   off it — there is currently no rate-limit header to read one off at all.
//! - A 15-request parallel burst on one token produced 11 × 200 and 4 ×
//!   429, with `Retry-After: 1` on every 429. The bucket scope is
//!   per-token (confirmed by watching `meta.remaining` drain across
//!   parallel calls sharing the token).
//!
//! Without proactive pacing rdc could rely on the existing reactive
//! [`crate::api::retry::send_with_retry`] handler — but every 429 wastes
//! a retry budget slot, churns logs, and (worse) the server retains the
//! request and only fails fast if `action: enforce`. Pacing client-side
//! keeps wide-fan-out operations (pull driver, deploy apply,
//! `parallel_fetch_by_id`) inside the cap from the first request, so
//! 429 is reserved for genuine contention (another rdc, the UI, or an
//! integration sharing the same token).
//!
//! The limiter is intentionally **per-client**, not global: each
//! `RossumClient` or `DataStorageClient` carries its own `Arc<RateLimiter>`
//! so all in-flight calls from one client share the same bucket, while two
//! clients of the same kind (two `RossumClient`s over different envs)
//! get independent buckets — matching the server's per-token scope. See the
//! bucket-lifetime note on [`RateLimiter::rossum_data_storage`] for how
//! often each kind of client (and therefore each bucket) gets rebuilt.

use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Bounded token bucket. Refills continuously at `refill_per_sec`,
/// capped at `capacity`. `acquire()` consumes one token, sleeping
/// (asynchronously) until one is available.
///
/// **Concurrency:** the inner state is behind a `tokio::sync::Mutex`.
/// On contention the lock is held only long enough to compute the wait
/// time, then released before sleeping — so a queue of N tasks
/// proceeds at the bucket's rate, not single-file behind one lock.
/// Fairness is approximate (tasks wake on sleep expiry and race for
/// the next token); for an HTTP client that's fine — request ordering
/// is not load-bearing.
pub struct RateLimiter {
    inner: Mutex<State>,
    capacity: f64,
    refill_per_sec: f64,
}

struct State {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// Bucket sized for Rossum's `default.core_api` policy: 10 tokens,
    /// refilling at 10/s. Use this for every `RossumClient` talking to
    /// the core API.
    pub fn rossum_core_api() -> Self {
        Self::new(10.0, 10.0)
    }

    /// Bucket for Rossum's **Data Storage** service (MDH). A different
    /// service from the core API, throttled independently on the same token:
    /// probes on 2026-08-24 saw 80 concurrent POSTs return 0 × 429 (~143
    /// req/s) and 320 requests at concurrency 20 sustain 36-63 req/s with a
    /// flat p95 and 0 × 429, *while* the core API 429'd at its 11th
    /// concurrent request on that same token.
    ///
    /// 30/s burst 30 is therefore a deliberate margin well under anything
    /// that throttled, not a measured ceiling — no rate-limit header is
    /// served by either service to read a real policy off (the
    /// `x-limiter-core-api` header quoted above is no longer present on
    /// responses, so re-verify before relying on it). If a cluster turns out
    /// to be stricter, this constant is the one line to change;
    /// [`crate::api::retry::send_with_retry`]'s `Retry-After` handling is the
    /// backstop underneath it.
    ///
    /// **Bucket lifetime.** `DataStorageClient::new` has exactly one
    /// production call site (inside `pull::mdh::list`, itself called once per
    /// `sync::run_cycle` invocation via `pull::common::list_remote`), so this
    /// bucket is rebuilt with a fresh full 30-token burst on every sync
    /// cycle. That is NOT an asymmetry with the core API's bucket: the core
    /// `RossumClient` is built at the top of `run_cycle` itself
    /// (`src/cli/sync/mod.rs`) and is just as fresh every cycle — `run_cycle`
    /// rebuilds its whole client/lockfile/catalog state from scratch on each
    /// call, and `watch::run_watch`'s loop calls it once per cycle; the
    /// renderer is the only object that loop explicitly carries across
    /// cycles. So under `--watch` or a multi-cycle sync, BOTH pacing layers
    /// get a fresh full burst every cycle, symmetrically. Harmless at the
    /// measured ~143 req/s ceiling either way; recorded here because it is
    /// the first thing that would matter if a cluster turned out to be
    /// stricter.
    pub fn rossum_data_storage() -> Self {
        Self::new(30.0, 30.0)
    }

    /// Build a custom-rate limiter. Initial token count = `capacity`
    /// (so the first burst of `capacity` requests proceeds immediately,
    /// matching the server's burst policy).
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            inner: Mutex::new(State {
                tokens: capacity,
                last_refill: Instant::now(),
            }),
            capacity,
            refill_per_sec,
        }
    }

    /// Take one token, sleeping until one is available. Cheap fast
    /// path when the bucket is non-empty (one lock acquire + arithmetic,
    /// no syscall).
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut state = self.inner.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(state.last_refill).as_secs_f64();
                state.tokens = (state.tokens + elapsed * self.refill_per_sec).min(self.capacity);
                state.last_refill = now;
                if state.tokens >= 1.0 {
                    state.tokens -= 1.0;
                    return;
                }
                // Compute deficit AFTER refill so we sleep exactly long
                // enough for one more token, not a full window.
                let deficit = 1.0 - state.tokens;
                Duration::from_secs_f64(deficit / self.refill_per_sec)
            };
            // Sleep OUTSIDE the lock so other tasks aren't blocked from
            // computing their own wait or fast-pathing a fresh token.
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn burst_drains_instantly_then_throttles() {
        // With capacity=10, the first 10 acquires return immediately;
        // the 11th must wait for one token to refill (100 ms at 10/s).
        let lim = RateLimiter::new(10.0, 10.0);
        let start = tokio::time::Instant::now();
        for _ in 0..10 {
            lim.acquire().await;
        }
        // Burst window: all 10 acquired without sleeping.
        assert!(start.elapsed() < Duration::from_millis(5));
        // 11th token requires waiting ~100ms.
        lim.acquire().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(99),
            "11th token should require ~100ms refill, got {:?}",
            elapsed,
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn sustained_rate_holds_at_refill_per_sec() {
        // After draining the burst, the steady-state rate must match
        // refill_per_sec: 20 more acquires at 10/s = ~2.0 s.
        let lim = RateLimiter::new(10.0, 10.0);
        for _ in 0..10 {
            lim.acquire().await; // burst
        }
        let start = tokio::time::Instant::now();
        for _ in 0..20 {
            lim.acquire().await;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1990) && elapsed <= Duration::from_millis(2100),
            "20 tokens at 10/s should take ~2s, got {:?}",
            elapsed,
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn concurrent_acquires_share_the_bucket() {
        // Spawn 30 tasks competing for one bucket; total elapsed time
        // is bounded by the rate: 30 tokens at 10/s starting with a
        // burst of 10 = 10 immediate + 20/10s = ~2 s.
        let lim = Arc::new(RateLimiter::new(10.0, 10.0));
        let start = tokio::time::Instant::now();
        let mut handles = Vec::new();
        for _ in 0..30 {
            let lim = lim.clone();
            handles.push(tokio::spawn(async move {
                lim.acquire().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1900) && elapsed <= Duration::from_millis(2200),
            "30 contending tokens at 10/s with burst 10 should take ~2s, got {:?}",
            elapsed,
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn data_storage_bucket_is_thirty_per_second_burst_thirty() {
        // Spec D1: 30/s, burst 30. Probed ceilings are far higher (S3: 80
        // concurrent → 0 × 429; S4: sustained 36-63 req/s → 0 × 429); 30 is a
        // deliberate margin, not the measured limit.
        let lim = RateLimiter::rossum_data_storage();
        let start = tokio::time::Instant::now();
        for _ in 0..30 {
            lim.acquire().await;
        }
        assert!(
            start.elapsed() < Duration::from_millis(5),
            "the first 30 must be a burst, took {:?}",
            start.elapsed(),
        );
        lim.acquire().await;
        assert!(
            start.elapsed() >= Duration::from_millis(33),
            "the 31st must wait one 1/30s refill, took {:?}",
            start.elapsed(),
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn core_bucket_is_untouched_by_the_data_storage_bucket() {
        // Spec D2/S2: the core policy is correctly calibrated at 10/s burst
        // 10 and must not drift when a second bucket is introduced.
        let lim = RateLimiter::rossum_core_api();
        let start = tokio::time::Instant::now();
        for _ in 0..10 {
            lim.acquire().await;
        }
        assert!(start.elapsed() < Duration::from_millis(5));
        lim.acquire().await;
        assert!(
            start.elapsed() >= Duration::from_millis(99),
            "core must still refill at 10/s, took {:?}",
            start.elapsed(),
        );
    }
}
