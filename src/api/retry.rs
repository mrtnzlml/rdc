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
//! - CONNECT failures (DNS resolution, connection refused/reset before the
//!   request is on the wire) get the same exponential backoff. See
//!   [`is_retriable_transport`] for why only connect-class errors qualify.
//!
//! Status codes NOT retried (returned to caller as-is):
//! - 4xx other than 429: auth/permission/not-found/method — retrying
//!   won't help.
//! - 500: usually a real server bug; retrying papers over it. The caller
//!   surfaces a useful error.

use anyhow::{Context, Result};

/// Optional renderer handle threaded through API calls. `None` silences
/// retry telemetry (production code passes `Some(log.clone())`; tests
/// that don't need output pass `None`).
pub type ProgressHandle = Option<std::sync::Arc<crate::log::Log>>;
use crate::api::rate_limit::RateLimiter;
use reqwest::{Response, StatusCode};
use std::sync::Arc;
use std::time::Duration;

/// Opt-in HTTP trace. Set `RDC_TRACE_HTTP=<path>` to append one CSV line per
/// HTTP **attempt** made by either client — the core API and Data Storage both
/// funnel through [`send_with_retry`], so one file captures the whole run:
///
/// ```text
/// epoch_ms,limiter_wait_ms,duration_ms,status,desc
/// ```
///
/// `limiter_wait_ms` is time spent blocked on the token bucket (`0.0` for a
/// client with no limiter), `duration_ms` is the round trip, `status` is the
/// HTTP status code or `ERR` for a transport failure. `desc` is last and is
/// **not** quoted — it may itself contain commas, so split on the first four.
///
/// The sink is a process-wide `OnceLock`: the variable is read once, on the
/// first request, and never re-read. Disabled (the default) costs one atomic
/// load per attempt and writes nothing.
mod trace {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};

    fn sink() -> Option<&'static Mutex<std::fs::File>> {
        static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
        SINK.get_or_init(|| {
            let path = std::env::var("RDC_TRACE_HTTP").ok()?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            Some(Mutex::new(file))
        })
        .as_ref()
    }

    /// Cheap gate so a disabled trace never formats a status string.
    pub fn enabled() -> bool {
        sink().is_some()
    }

    pub fn record(limiter_wait_ms: f64, duration_ms: f64, status: &str, desc: &str) {
        let Some(sink) = sink() else { return };
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        // A poisoned lock or a failed write must never take down a sync —
        // this is diagnostics, not state.
        if let Ok(mut file) = sink.lock() {
            let _ = writeln!(
                file,
                "{epoch_ms:.1},{limiter_wait_ms:.1},{duration_ms:.1},{status},{desc}"
            );
        }
    }
}

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
        match send_once(&mut build, desc, limiter, attempt + 1).await {
            Ok(resp) => {
                let Some(reason) = retriable_reason(resp.status()) else {
                    return Ok(resp);
                };
                // Retries are an internal concern: a terminal failure after
                // MAX_ATTEMPTS surfaces as an error from the final
                // `build().send()` call below. No user-facing retry chatter.
                let _ = (reason, &progress);
                let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                tokio::time::sleep(wait).await;
            }
            // A connect-class failure never reached the server, so it is
            // retriable for any method — including POST. Everything else is
            // returned as-is.
            Err(e) if is_retriable_transport(&e) => {
                tokio::time::sleep(backoff(attempt)).await;
            }
            Err(e) => return Err(e),
        }
    }
    send_once(&mut build, desc, limiter, MAX_ATTEMPTS).await
}

/// One rate-limited attempt, traced when `RDC_TRACE_HTTP` is set. Split out of
/// [`send_with_retry`] so the retry loop and the final attempt share exactly
/// one send path — and therefore one trace point.
async fn send_once(
    build: &mut impl FnMut() -> reqwest::RequestBuilder,
    desc: &str,
    limiter: Option<&Arc<RateLimiter>>,
    attempt: u32,
) -> Result<Response> {
    let gate = std::time::Instant::now();
    if let Some(l) = limiter {
        l.acquire().await;
    }
    let limiter_wait_ms = gate.elapsed().as_secs_f64() * 1000.0;
    let sent = std::time::Instant::now();
    let out = build()
        .send()
        .await
        .with_context(|| format!("{desc} (attempt {attempt})"));
    if trace::enabled() {
        let status = match &out {
            Ok(r) => r.status().as_str().to_string(),
            Err(_) => "ERR".to_string(),
        };
        trace::record(
            limiter_wait_ms,
            sent.elapsed().as_secs_f64() * 1000.0,
            &status,
            desc,
        );
    }
    out
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

/// Whether a transport failure (no HTTP response at all) is worth retrying.
///
/// **Connect-class only, deliberately.** `is_connect()` means the connection
/// was never established — DNS did not resolve, the peer refused, the socket
/// reset during the handshake — so the server never saw the request and
/// re-sending it cannot duplicate anything. That makes it safe for POST as
/// well as for reads.
///
/// A read TIMEOUT is excluded for exactly that reason: the request may well
/// have arrived and been applied, with only the response lost. Retrying a
/// POST there would create the object twice, which is a far worse outcome
/// than the error it would paper over.
///
/// Why this exists: a single DNS blip used to abort an entire `sync`. Observed
/// three times in one session against the Data Storage host, whose 30 req/s
/// bucket makes it the most resolver-intensive thing rdc talks to — and it is
/// the unattended CI deploy (`rdc sync --allow-deletes --yes`) that pays for
/// it, since nobody is there to re-run.
fn is_retriable_transport(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|c| c.downcast_ref::<reqwest::Error>())
        .any(|e| e.is_connect())
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
    /// A real connect failure — nothing is listening on port 1 — must
    /// classify as retriable. Constructed rather than mocked so the test
    /// exercises the actual `reqwest::Error` variant the runtime produces.
    #[tokio::test]
    async fn connect_failures_are_retriable() {
        let err = reqwest::Client::new()
            .get("http://127.0.0.1:1/nothing-here")
            .send()
            .await
            .expect_err("connecting to a closed port must fail");
        assert!(err.is_connect(), "precondition: this is a connect error");
        let wrapped = anyhow::Error::new(err).context("listing indexes for 'x'");
        assert!(
            is_retriable_transport(&wrapped),
            "a connect failure must be retriable even through added context"
        );
    }

    /// A read timeout must NOT be retried: the request may already have been
    /// applied server-side with only the response lost, so re-sending a POST
    /// would create the object twice.
    #[tokio::test]
    async fn read_timeouts_are_not_retriable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept the connection but never answer, so the client times out
        // AFTER the request is on the wire.
        std::thread::spawn(move || {
            let _keep = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(5));
        });
        let err = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .timeout(Duration::from_millis(250))
            .send()
            .await
            .expect_err("the server never responds");
        assert!(err.is_timeout(), "precondition: this is a timeout, not a connect error");
        assert!(
            !is_retriable_transport(&anyhow::Error::new(err)),
            "a timeout must NOT be retried — the write may already have landed"
        );
    }

    /// A non-transport error carries no `reqwest::Error` at all.
    #[test]
    fn unrelated_errors_are_not_retriable() {
        assert!(!is_retriable_transport(&anyhow::anyhow!("parsing lockfile")));
    }
    /// The predicate above is only half the fix — this proves the LOOP is
    /// wired to it: a connect failure must be re-sent, not returned on the
    /// first try. Counted through the `build` closure, which
    /// [`send_with_retry`] invokes once per attempt.
    ///
    /// `start_paused` lets tokio auto-advance the 1s/2s/4s/8s backoff, so the
    /// full retry ladder runs instantly instead of taking 15 seconds.
    #[tokio::test(start_paused = true)]
    async fn a_connect_failure_is_retried_up_to_max_attempts() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let client = reqwest::Client::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();

        let result = send_with_retry(
            || {
                counter.fetch_add(1, Ordering::SeqCst);
                client.get("http://127.0.0.1:1/nothing-here")
            },
            "probe",
            None,
            None,
        )
        .await;

        assert!(result.is_err(), "every attempt fails, so the call must error");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            MAX_ATTEMPTS as usize,
            "a connect failure must be retried the full ladder, not surfaced \
             on the first attempt"
        );
    }
}
