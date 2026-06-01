//! HTTP retry-with-backoff helper for transient errors (spec §13).
//!
//! Used by both `RossumClient` and `DataStorageClient`. The Rossum API
//! sometimes returns 429 under load, and concurrent pulls make that more
//! likely. Transient gateway errors (502/503/504) are also retried.
//!
//! Strategy:
//!
//! - Up to 5 attempts total (4 retries) for both 429 and 5xx-transient.
//! - On 429: sleep for `Retry-After` seconds if the API gave us one,
//!   otherwise exponential backoff (1s, 2s, 4s, 8s, 16s).
//! - On 502/503/504: same exponential backoff. (`Retry-After` may also
//!   appear on 503; honored when present.)
//! - Caps at 60s per sleep to avoid worst-case stalls.
//! - Stderr line per retry so users see the tool isn't hung.
//!
//! Status codes NOT retried (returned to caller as-is):
//! - 4xx other than 429: auth/permission/not-found/method — retrying
//!   won't help.
//! - 500: usually a real server bug; retrying papers over it. The caller
//!   surfaces a useful error.
//! - Network errors before a response arrives: surfaced via `?` from
//!   `.send()`. Reqwest's own connect retry is not exposed; if needed in
//!   the future, layer it on the `build()` closure side.

use anyhow::{Context, Result};

/// Optional renderer handle threaded through API calls. `None` silences
/// retry telemetry (production code passes `Some(log.clone())`; tests
/// that don't need output pass `None`).
pub type ProgressHandle = Option<std::sync::Arc<crate::log::Log>>;
use crate::api::rate_limit::RateLimiter;
use reqwest::{Response, StatusCode};
use std::sync::Arc;
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 5;
const MAX_SLEEP_SECS: u64 = 60;

/// Send a request, retrying with backoff on 429 / 502 / 503 / 504. Caller
/// passes a closure that produces a fresh `RequestBuilder` each attempt
/// (since `RequestBuilder` is consumed by `.send()`).
///
/// Retries are silent on the user side: any active progress line is left as-is
/// during backoff, and a terminal failure after `MAX_ATTEMPTS` surfaces as a
/// normal error. The `progress` parameter is reserved for future use (e.g.
/// structured retry telemetry) and is otherwise unused — callers may pass
/// `None`.
pub async fn send_with_retry(
    mut build: impl FnMut() -> reqwest::RequestBuilder,
    desc: &str,
    progress: ProgressHandle,
    limiter: Option<&Arc<RateLimiter>>,
) -> Result<Response> {
    // Retries up to MAX_ATTEMPTS - 1 times; the final pass falls through
    // to the post-loop send so the function always returns from one
    // explicit `send()` call.
    //
    // The rate limiter acquires one token PER ATTEMPT, not just once
    // before the loop. Retries (e.g. after a 429) sleep for
    // `Retry-After` first, which gives the local bucket time to refill;
    // taking a fresh token before the re-send keeps the proactive cap
    // accurate even when several requests are mid-retry.
    for attempt in 0..MAX_ATTEMPTS - 1 {
        if let Some(l) = limiter {
            l.acquire().await;
        }
        let resp = build()
            .send()
            .await
            .with_context(|| format!("{desc} (attempt {})", attempt + 1))?;
        let Some(reason) = retriable_reason(resp.status()) else {
            return Ok(resp);
        };
        // Retries are an internal concern: a terminal failure after
        // MAX_ATTEMPTS surfaces as an error from the final `build().send()`
        // call below. No user-facing retry chatter.
        let _ = (reason, &progress);
        let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
        tokio::time::sleep(wait).await;
    }
    if let Some(l) = limiter {
        l.acquire().await;
    }
    build()
        .send()
        .await
        .with_context(|| format!("{desc} (attempt {})", MAX_ATTEMPTS))
}

/// Returns the human-readable reason a status is retriable, or None if not.
fn retriable_reason(status: StatusCode) -> Option<&'static str> {
    match status {
        StatusCode::TOO_MANY_REQUESTS => Some("rate limited"),
        StatusCode::BAD_GATEWAY => Some("bad gateway"),
        StatusCode::SERVICE_UNAVAILABLE => Some("service unavailable"),
        StatusCode::GATEWAY_TIMEOUT => Some("gateway timeout"),
        _ => None,
    }
}

fn retry_after(resp: &Response) -> Option<Duration> {
    let header = resp.headers().get("Retry-After")?.to_str().ok()?;
    // Rossum sends seconds as an integer; some APIs send HTTP-date too,
    // but we only need the seconds form here.
    let secs: u64 = header.parse().ok()?;
    Some(Duration::from_secs(secs.min(MAX_SLEEP_SECS)))
}

fn backoff(attempt: u32) -> Duration {
    // 1s, 2s, 4s, 8s, 16s — capped at 60s.
    let secs = 1u64.checked_shl(attempt).unwrap_or(MAX_SLEEP_SECS);
    Duration::from_secs(secs.min(MAX_SLEEP_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_with_cap() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(4), Duration::from_secs(16));
        // Very high attempt counts saturate at the cap.
        assert_eq!(backoff(60), Duration::from_secs(MAX_SLEEP_SECS));
    }

    #[test]
    fn retriable_classification() {
        assert_eq!(retriable_reason(StatusCode::TOO_MANY_REQUESTS), Some("rate limited"));
        assert_eq!(retriable_reason(StatusCode::BAD_GATEWAY), Some("bad gateway"));
        assert_eq!(retriable_reason(StatusCode::SERVICE_UNAVAILABLE), Some("service unavailable"));
        assert_eq!(retriable_reason(StatusCode::GATEWAY_TIMEOUT), Some("gateway timeout"));
        // Not retriable.
        assert_eq!(retriable_reason(StatusCode::OK), None);
        assert_eq!(retriable_reason(StatusCode::UNAUTHORIZED), None);
        assert_eq!(retriable_reason(StatusCode::FORBIDDEN), None);
        assert_eq!(retriable_reason(StatusCode::NOT_FOUND), None);
        assert_eq!(retriable_reason(StatusCode::METHOD_NOT_ALLOWED), None);
        // 500 is intentionally NOT retried — usually a real server bug.
        assert_eq!(retriable_reason(StatusCode::INTERNAL_SERVER_ERROR), None);
    }
}
