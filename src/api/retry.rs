//! HTTP retry-with-backoff helper for 429 Too Many Requests (M30).
//!
//! Used by both `RossumClient` and `DataStorageClient`. The Rossum API
//! sometimes returns 429 under load, and concurrent pulls (introduced in
//! M30 alongside this) make that more likely. Strategy:
//!
//! - Up to 5 attempts total (4 retries).
//! - On 429, sleep for `Retry-After` seconds if the API gave us one,
//!   otherwise exponential backoff (1s, 2s, 4s, 8s).
//! - Caps at 60s per sleep to avoid worst-case stalls.
//! - Stderr line per retry so users see the tool isn't hung.
//!
//! Other status codes (4xx other than 429, 5xx) are returned to the
//! caller as-is — they handle 401/403/404/405 differently.

use anyhow::{Context, Result};
use reqwest::{Response, StatusCode};
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 5;
const MAX_SLEEP_SECS: u64 = 60;

/// Send a request, retrying with backoff on 429. Caller passes a closure
/// that produces a fresh `RequestBuilder` each attempt (since
/// `RequestBuilder` is consumed by `.send()`).
pub async fn send_with_retry(
    mut build: impl FnMut() -> reqwest::RequestBuilder,
    desc: &str,
) -> Result<Response> {
    for attempt in 0..MAX_ATTEMPTS {
        let resp = build()
            .send()
            .await
            .with_context(|| format!("{desc} (attempt {})", attempt + 1))?;
        if resp.status() == StatusCode::TOO_MANY_REQUESTS && attempt < MAX_ATTEMPTS - 1 {
            let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
            eprintln!(
                "rate limited (429) on {desc}; retrying in {}s (attempt {}/{})",
                wait.as_secs(),
                attempt + 1,
                MAX_ATTEMPTS,
            );
            tokio::time::sleep(wait).await;
            continue;
        }
        return Ok(resp);
    }
    unreachable!("loop exits via return on the last attempt")
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
}
