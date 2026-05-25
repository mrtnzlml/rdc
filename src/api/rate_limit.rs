//! Client-side token-bucket rate limiter for [`crate::api::RossumClient`].
//!
//! Rossum's ingress rate limiter enforces `default.core_api` at
//! **10 req/s with burst 10** (window 1 s). Empirically verified against
//! `api.elis.rossum.ai/v1` on 2026-05-22:
//!
//! - The `x-limiter-core-api` header on 200 responses reports
//!   `{"config":{"rate_limit":10,"burst":10,"window":1,"action":"enforce"}}`.
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
//! The limiter is intentionally **per-`RossumClient`**, not global: each
//! client carries an `Arc<RateLimiter>` so all in-flight calls from one
//! client share the same bucket while two clients (e.g. `rdc deploy`'s
//! src + tgt) get independent buckets — matching the server's
//! per-token scope.

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
}
