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
use reqwest::{Response, StatusCode};
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 5;
const MAX_SLEEP_SECS: u64 = 60;

/// Send a request, retrying with backoff on 429 / 502 / 503 / 504. Caller
/// passes a closure that produces a fresh `RequestBuilder` each attempt
/// (since `RequestBuilder` is consumed by `.send()`).
///
/// `progress` — when `Some`, retry warnings are printed via
/// `progress.suspend()` so the progress bar isn't corrupted. Pass `None`
/// when no progress bar is active (e.g. `rdc auth`, `rdc diff`).
pub async fn send_with_retry(
    mut build: impl FnMut() -> reqwest::RequestBuilder,
    desc: &str,
    progress: Option<&crate::progress::KindProgress>,
) -> Result<Response> {
    for attempt in 0..MAX_ATTEMPTS {
        let resp = build()
            .send()
            .await
            .with_context(|| format!("{desc} (attempt {})", attempt + 1))?;
        if let Some(reason) = retriable_reason(resp.status()) {
            if attempt < MAX_ATTEMPTS - 1 {
                let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                let msg = format!(
                    "{reason} ({}) on {desc}; retrying in {}s (attempt {}/{})",
                    resp.status().as_u16(),
                    wait.as_secs(),
                    attempt + 1,
                    MAX_ATTEMPTS,
                );
                match progress {
                    Some(p) => p.suspend(move || eprintln!("{msg}")),
                    None => eprintln!("{msg}"),
                }
                tokio::time::sleep(wait).await;
                continue;
            }
        }
        return Ok(resp);
    }
    unreachable!("loop exits via return on the last attempt")
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
