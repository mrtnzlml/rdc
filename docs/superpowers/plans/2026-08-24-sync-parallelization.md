# Faster `rdc sync` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut `rdc sync` wall-clock by saturating each service's own rate budget — a second token bucket for Data Storage, concurrent MDH index/row fetches, MDH listing overlapped with core listing, and a concurrent-network / sequential-apply split on the push path — without changing what any command requests, prompts, or writes.

**Architecture:** Two independent token buckets, one per service (core API 10/s, Data Storage 30/s), both `Arc<RateLimiter>` behind the single `retry::send_with_retry` chokepoint. Every hot loop keeps its *decision* logic sequential and moves only its *network* stage into a bounded `buffer_unordered` / `buffered` fan-out. The bucket — never the fan-out constant — is the throughput governor. Interactive prompts, lockfile mutation and filesystem writes stay on the sequential path by construction.

**Tech Stack:** Rust (edition 2024), `futures 0.3` (`buffer_unordered` for read paths, `buffered` for the order-preserving push path), `tokio 1` (`try_join!`, `test-util`'s `start_paused` clock), `wiremock 0.6` + `assert_cmd` for integration tests.

**Spec:** `docs/superpowers/specs/2026-08-24-sync-parallelization-design.md`

## Global Constraints

- **Do not change what a command requests.** Every task must leave the per-command HTTP request count identical on the success path. The one documented exception is Task 9–12's error path (see D10): on a mid-batch failure, requests already dispatched concurrently still complete. Nothing else may add or drop a request.
- **The bucket governs, not the fan-out.** `MDH_FANOUT = 10` and `PUSH_FANOUT = 5` exist only to bound outstanding requests. Do not "tune" them to chase throughput — raise the bucket rate instead, and only with a measurement.
- Core limiter stays **10 req/s, burst 10** (`RateLimiter::rossum_core_api()`). Spec S2 verified it is correctly calibrated. Do not touch it.
- Data Storage limiter is **30 req/s, burst 30**. Measured ceiling is far higher (S3: 80 concurrent → 0 × 429; S4: sustained 36–63 req/s → 0 × 429); 30 is a deliberate margin. It is one constant to change if a cluster turns out to be stricter.
- **Sync's four safety layers are untouched.** Classifier (offline), resolver, push-side drift check, defensive last-mile hash compare. Layer 3's per-item drift check stays **per item** — only its scheduling changes. If a task finds itself changing *whether* a check runs, stop: that is out of scope.
- **Creates stay sequential.** `POST` assigns ids that later items resolve against. Only the update/PATCH path is parallelized.
- **A drifted item is never PATCHed concurrently.** It returns a needs-prompt marker and is resolved on the sequential pass, so `resolve_push_drift` prompts can never interleave. `stdin_coord` remains the single stdin owner.
- **Deletes are not parallelized** (`push::deletes::run_deletes` is cascade-ordered; spec N3).
- **Do not reduce the number of requests.** Spec N1/N2: core listing is already at its ceiling and a change-gate on MDH index fetches would trade away detection of a remote-only index edit. Both are separate, safety-bearing designs.
- Never put a customer name or customer-specific identifier — org/division/region code, real env name, queue/engine/hook/dataset slug, hostname, URL or file path — in code, tests, fixtures, docs **or commit messages**. Use `acme`, `main`, `invoices`, `dev`/`test`/`prod`, `gl-codes`, `vendors`.
- **Never `git push`.** Commit to local `main`. The user publishes.
- **This working tree is shared with another worker.** Always `git add` with an explicit pathspec — never `git add -A` or `git commit -a`.
- **Build economy:** this crate compiles slowly. Per task run only the filtered test (`cargo test -p rdc --locked --lib <filter>` or `cargo test -p rdc --locked --test <bin> <filter>`). The full suite runs once, in Task 13. Never start a rebuild while an integration suite is running — it swaps `target/debug/rdc` under the tests that spawn it.
- **`cargo fmt` is not clean in this repo** under the local rustfmt (pre-existing skew). Never run repo-wide `cargo fmt`. Match surrounding style by hand.
- Lint gate: `cargo clippy -p rdc --all-targets --locked -- -D warnings` must pass for every task's touched files.

---

## File Structure

**New files**

| File | Responsibility |
|---|---|
| `tests/http_trace.rs` | Integration binary owning the `RDC_TRACE_HTTP` test. Must contain exactly one test — the trace sink is a process-wide `OnceLock`. |
| `src/cli/push/concurrent.rs` | The push fan-out primitive: `PUSH_FANOUT`, `Prepared<T>`, `prepare_all`. No driver logic. |

**Modified files**

| File | Change |
|---|---|
| `src/api/retry.rs` | Extract `send_once`; add the `trace` module (D11). |
| `src/api/rate_limit.rs` | Add `rossum_data_storage()` (D1). |
| `src/api/data_storage.rs` | `DataStorageClient` gains an `Arc<RateLimiter>`; `send_envelope` passes it (D1). |
| `src/cli/pull/mdh.rs` | `fetch_index_sets` helper (D3/D4); `plan_mdh_index_edits` uses it (D4); `MdhListed` gains `slugs` + `index_sets` + `new()` + `datasets()` (D6/D7); `process` consumes the prefetch and fans out row pulls (D6/D8). |
| `src/cli/pull/common.rs` | MDH listing becomes a `tokio::join!` sibling of the core stream and prefetches index sets (D5/D6). |
| `src/cli/sync/execute.rs` | `slug_to_collection` reads `catalog.mdh.datasets()`; the `MdhListed` reconstruction becomes a `.clone()` (D7). |
| `src/cli/sync/mod.rs` | Test-only `MdhListed` construction moves to `MdhListed::new` (D7). |
| `src/cli/push/mod.rs` | `mod concurrent;` |
| `src/cli/push/{rules,hooks,labels,engines,engine_fields,queues,email_templates,schemas,workspaces,inboxes}.rs` | Update path splits into a concurrent network stage and a sequential apply stage (D9/D10). |

**Task order rationale.** Tasks 1–2 land the measurement tool and the pacing that every later task depends on. Tasks 3–8 are the read path, ordered so each is independently shippable: 3 (helper) → 4 (the biggest measured defect, B9) → 5 (slug map, a prerequisite for 7) → 6 (overlap) → 7 (prefetch) → 8 (row fan-out). Tasks 9–12 are the write path, pattern-setter first. Task 13 verifies and publishes numbers.

---

### Task 1: `RDC_TRACE_HTTP` — opt-in per-attempt HTTP trace

Spec **D11**. Every number in the spec was produced by an uncommitted version of this. Making it a real feature is what lets Task 13 verify D1–D10 and lets a future regression be diagnosed rather than guessed at. It also collapses the duplicated send-and-context block at the bottom of `send_with_retry`.

**Files:**
- Modify: `src/api/retry.rs`
- Create: `tests/http_trace.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: no new public API. The observable contract is the environment variable `RDC_TRACE_HTTP=<path>` and the CSV line format `epoch_ms,limiter_wait_ms,duration_ms,status,desc`. Task 13 parses that format.

- [ ] **Step 1: Write the failing test**

Create `tests/http_trace.rs`:

```rust
//! `RDC_TRACE_HTTP` — the opt-in per-attempt HTTP trace in `api::retry`.
//!
//! This MUST stay the only test in this binary. The trace sink is a
//! process-wide `OnceLock` initialised from the environment on the first
//! request, so a second test could neither observe the disabled state nor
//! redirect the sink — and `set_var` is only sound while no other thread is
//! reading the environment.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn trace_writes_one_csv_line_per_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("http.csv");

    // SAFETY: the only test in this binary, and nothing has spawned a thread
    // that reads the environment yet (the mock server starts below).
    unsafe { std::env::set_var("RDC_TRACE_HTTP", &trace_path) };

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    // 429 with `Retry-After: 0` is retriable and sleeps for zero seconds, so
    // this burns every attempt without making the test slow.
    Mock::given(method("GET"))
        .and(path("/throttled"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .mount(&server)
        .await;

    let http = reqwest::Client::new();

    let ok_url = format!("{}/ok", server.uri());
    let r = rdc::api::retry::send_with_retry(|| http.get(&ok_url), "GET /ok", None, None)
        .await
        .expect("the 200 request must succeed");
    assert_eq!(r.status(), 200);

    let throttled_url = format!("{}/throttled", server.uri());
    let r = rdc::api::retry::send_with_retry(
        || http.get(&throttled_url),
        "GET /throttled",
        None,
        None,
    )
    .await
    .expect("a retriable status is returned, not an error");
    assert_eq!(r.status(), 429);

    let body = std::fs::read_to_string(&trace_path)
        .expect("setting RDC_TRACE_HTTP must create the trace file");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(
        lines.len(),
        6,
        "one line per ATTEMPT: 1 for /ok + 5 for /throttled, got {lines:?}"
    );

    // `desc` is the last field and is not quoted, so split into exactly 5.
    let fields: Vec<Vec<&str>> = lines.iter().map(|l| l.splitn(5, ',').collect()).collect();
    for f in &fields {
        assert_eq!(f.len(), 5, "epoch_ms,limiter_wait_ms,duration_ms,status,desc");
        assert!(
            f[0].parse::<f64>().unwrap() > 1_700_000_000_000.0,
            "epoch_ms must be a real wall-clock millisecond stamp, got {}",
            f[0]
        );
        assert_eq!(f[1], "0.0", "no limiter was passed, so no gate wait");
        assert!(f[2].parse::<f64>().unwrap() >= 0.0, "duration_ms must parse");
    }
    assert_eq!((fields[0][3], fields[0][4]), ("200", "GET /ok"));
    for f in &fields[1..] {
        assert_eq!((f[3], f[4]), ("429", "GET /throttled"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --locked --test http_trace`
Expected: FAIL — the trace file is never created, so `read_to_string` panics with "setting RDC_TRACE_HTTP must create the trace file".

- [ ] **Step 3: Write the implementation**

In `src/api/retry.rs`, add the module immediately after the existing `use` block and before `const MAX_ATTEMPTS`:

```rust
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
```

Then replace the two near-identical send blocks in `send_with_retry` with calls to one traced helper. The loop body becomes:

```rust
    for attempt in 0..MAX_ATTEMPTS - 1 {
        let resp = send_once(&mut build, desc, limiter, attempt + 1).await?;
        let Some(reason) = retriable_reason(resp.status()) else {
            return Ok(resp);
        };
```

and the tail (everything from the second `if let Some(l) = limiter` to the end of the function) becomes:

```rust
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
```

`build` changes from `mut build: impl FnMut() -> reqwest::RequestBuilder` to being passed as `&mut build`; the parameter declaration in `send_with_retry` stays exactly as it is.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rdc --locked --test http_trace`
Expected: PASS.

Then confirm the retry-loop refactor did not change retry behaviour:

Run: `cargo test -p rdc --locked --lib api::retry`
Expected: PASS (`backoff_doubles_with_cap`, `retriable_classification`, and the rest of the module's tests).

- [ ] **Step 5: Document the flag in the spec's terms**

Append to the `## Verified facts` intro paragraph in `docs/superpowers/specs/2026-08-24-sync-parallelization-design.md` — replace the words "the real release binary traced through `retry::send_with_retry`" with "the real release binary traced through `RDC_TRACE_HTTP` (see D11)". No other spec edit.

- [ ] **Step 6: Commit**

```bash
git add src/api/retry.rs tests/http_trace.rs docs/superpowers/specs/2026-08-24-sync-parallelization-design.md
git commit -m "feat(api): RDC_TRACE_HTTP opt-in per-attempt HTTP trace

One CSV line per attempt through the single send chokepoint, behind a
OnceLock so a disabled trace costs one atomic load. Collapses the
duplicated send-and-context block into send_once."
```

---

### Task 2: A second token bucket for Data Storage

Spec **D1**, **D2**. Data Storage is a different service from the core API, throttles independently on the same token (S5), and today has no client-side limiter at all (C1) — it carries 72% of a sync's requests unpaced. Give it its own bucket at a rate calibrated to the probes, and leave the core bucket alone.

Building the limiter inside `DataStorageClient::new` means **zero churn at its 36 call sites** — backward compatible by construction.

**Files:**
- Modify: `src/api/rate_limit.rs`
- Modify: `src/api/data_storage.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub fn RateLimiter::rossum_data_storage() -> Self` (30 tokens, 30/s). `DataStorageClient` gains a private `limiter: Arc<RateLimiter>` field; its public constructor signature `new(base_url: String, token: String) -> Result<Self>` is **unchanged**. No later task calls the limiter directly.

- [ ] **Step 1: Write the failing tests**

Append inside the existing `#[cfg(test)] mod tests` in `src/api/rate_limit.rs`:

```rust
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
```

Append inside the existing `#[cfg(test)] mod tests` in `src/api/data_storage.rs`:

```rust
    /// Spec D1: every Data Storage request goes through the client's own
    /// bucket. 40 concurrent list calls on one client must take at least the
    /// bucket's own floor — 30 immediate + 10 more at 30/s = 333ms — proving
    /// the limiter is actually threaded into `send_envelope` and that clones
    /// share it. Real clock: `reqwest` does real IO, so the paused-time trick
    /// used in `rate_limit`'s unit tests does not apply here.
    #[tokio::test(flavor = "multi_thread")]
    async fn data_storage_requests_are_paced_by_the_client_bucket() {
        use futures::stream::StreamExt;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TEST".to_string()).unwrap();
        let start = std::time::Instant::now();
        // Clone per task: a clone must share the bucket, not get a fresh one.
        let results: Vec<Result<Vec<Value>>> = futures::stream::iter(0..40)
            .map(|_| {
                let c = client.clone();
                async move { c.list_indexes("vendors", None).await }
            })
            .buffer_unordered(40)
            .collect()
            .await;
        assert!(results.iter().all(|r| r.is_ok()), "all 40 must succeed");
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(300),
            "40 requests through a 30/s burst-30 bucket cannot finish faster \
             than ~333ms; took {:?} — the limiter is not wired in",
            start.elapsed(),
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rdc --locked --lib rate_limit::tests::data_storage_bucket_is_thirty_per_second_burst_thirty`
Expected: FAIL to compile — "no function or associated item named `rossum_data_storage`".

Run: `cargo test -p rdc --locked --lib data_storage::tests::data_storage_requests_are_paced_by_the_client_bucket`
Expected: FAIL — 40 unpaced requests against a local mock finish in well under 300ms, so the elapsed assertion fires.

- [ ] **Step 3: Write the implementation**

In `src/api/rate_limit.rs`, add after `rossum_core_api()`:

```rust
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
    pub fn rossum_data_storage() -> Self {
        Self::new(30.0, 30.0)
    }
```

In `src/api/data_storage.rs`, add the field and build it in `new`:

```rust
#[derive(Clone)]
pub struct DataStorageClient {
    base_url: String,
    token: String,
    http: Client,
    /// Shared across clones so one logical client keeps one bucket, matching
    /// the server's per-token scope (see [`RateLimiter::rossum_data_storage`]).
    limiter: Arc<RateLimiter>,
}
```

with `use crate::api::rate_limit::RateLimiter;` and `use std::sync::Arc;` added to the imports at the top of the file, and:

```rust
    pub fn new(base_url: String, token: String) -> Result<Self> {
        let http = crate::api::build_http_client()?;
        Ok(Self {
            base_url,
            token,
            http,
            limiter: Arc::new(RateLimiter::rossum_data_storage()),
        })
    }
```

Then in `send_envelope`, replace the "no client-side limiter here" comment and the `None` argument:

```rust
        // Data Storage is a separate service from the core API and throttles
        // independently on the same token, so it gets its OWN bucket rather
        // than spending core tokens on calls nobody asked us to pace.
        let resp = crate::api::retry::send_with_retry(
            || self.http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.token))
                .json(&body),
            &format!("POST {url}"),
            progress,
            Some(&self.limiter),
        ).await?;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --locked --lib rate_limit::`
Expected: PASS — all four bucket tests including the two new ones.

Run: `cargo test -p rdc --locked --lib data_storage::`
Expected: PASS.

- [ ] **Step 5: Verify no call site broke**

Run: `cargo build -p rdc --locked`
Expected: builds clean — `DataStorageClient::new`'s signature is unchanged, so all 36 call sites compile untouched.

Run: `cargo clippy -p rdc --all-targets --locked -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/api/rate_limit.rs src/api/data_storage.rs
git commit -m "feat(api): pace Data Storage with its own 30/s token bucket

The MDH service throttles independently of the core API on the same
token (80 concurrent POSTs -> 0 x 429 while core 429s at its 11th), and
carried 72% of a sync's requests entirely unpaced. Built inside
DataStorageClient::new, so no call site changes."
```

---
### Task 3: `fetch_index_sets` — one shared, doubly-concurrent index fetcher

Spec **D3**, **D4**. Two independent Data Storage calls per dataset are awaited in sequence (C2), and the only two places that need index sets each schedule them their own way. This task builds the single helper and switches `process` (the pull write path) onto it. Task 5 switches the dry-run forecast onto the same helper.

Spec S6 is why this is the whole lever: `indexes/list` and `search_indexes/list` each **require** a `collectionName` (422 without it) and `collections/list` returns only the implicit `_id_` index, so **2 calls per dataset is a floor** with no bulk form to optimise into. The only thing left to change is when they are issued.

**Files:**
- Modify: `src/cli/pull/mdh.rs` (rewrite `fetch_index_set`, add `MDH_FANOUT` + `fetch_index_sets`, rewrite sub-phase B of `process`, add tests)

**Interfaces:**
- Consumes: Task 2's Data Storage bucket (it is what actually paces these calls).
- Produces:
  - `pub(crate) const MDH_FANOUT: usize = 10;`
  - `pub(crate) async fn fetch_index_sets(client: &DataStorageClient, wanted: &[(String, String)], progress: &Arc<Log>) -> Result<BTreeMap<String, IndexSet>>` — `wanted` is `(dataset_slug, collection_name)`, the map is keyed by dataset slug and contains **every** requested slug. Tasks 5 and 7 both call it.
  - `fetch_index_set` keeps its existing private signature `(&DataStorageClient, &str, &Arc<Log>) -> Result<IndexSet>`.

- [ ] **Step 1: Write the failing tests**

Append inside the existing `#[cfg(test)] mod tests` in `src/cli/pull/mdh.rs`:

```rust
    /// Spec D3: the regular and search index listings for ONE dataset are
    /// independent, so they must overlap. With both mocks delayed 200ms, a
    /// sequential fetch costs ~400ms and a joined one ~200ms.
    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_index_set_overlaps_regular_and_search() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let delayed = |body: serde_json::Value| {
            ResponseTemplate::new(200)
                .set_body_json(body)
                .set_delay(std::time::Duration::from_millis(200))
        };
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(delayed(
                serde_json::json!({ "code": "ok", "result": [ { "name": "acct" } ] }),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(delayed(serde_json::json!({ "code": "ok", "result": [] })))
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TEST".to_string()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let start = std::time::Instant::now();
        let set = fetch_index_set(&client, "gl-codes", &progress).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(set.regular.len(), 1, "regular indexes must still be decoded");
        assert!(set.search.is_empty(), "search indexes must still be decoded");
        assert!(
            elapsed < std::time::Duration::from_millis(350),
            "the two listings must overlap; sequential would be ~400ms, took {elapsed:?}",
        );
    }

    /// Spec D4: `fetch_index_sets` fans out across datasets, keys the result by
    /// DATASET SLUG (not collection name), returns every requested slug, and
    /// issues exactly the 2 calls per dataset that S6 makes a floor — no more,
    /// no fewer.
    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_index_sets_keys_by_slug_and_costs_two_calls_per_dataset() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": [ { "name": "acct" } ] }),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TEST".to_string()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        // Slug deliberately differs from the collection name so a helper that
        // keyed by name would fail this.
        let wanted = vec![
            ("gl-codes".to_string(), "GL Codes".to_string()),
            ("vendors".to_string(), "vendors".to_string()),
        ];
        let sets = fetch_index_sets(&client, &wanted, &progress).await.unwrap();

        assert_eq!(
            sets.keys().collect::<Vec<_>>(),
            vec!["gl-codes", "vendors"],
            "the map must be keyed by dataset slug"
        );
        assert_eq!(sets["gl-codes"].regular.len(), 1);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            4,
            "2 datasets x 2 calls (S6 floor) — no bulk form exists to do better"
        );

        // An empty batch must not touch the network at all.
        let empty = fetch_index_sets(&client, &[], &progress).await.unwrap();
        assert!(empty.is_empty());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            4,
            "an empty batch must issue no requests"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rdc --locked --lib pull::mdh::tests::fetch_index_set`
Expected: `fetch_index_set_overlaps_regular_and_search` FAILS on the elapsed assertion (~400ms sequential); `fetch_index_sets_keys_by_slug_and_costs_two_calls_per_dataset` FAILS to compile — "cannot find function `fetch_index_sets`".

- [ ] **Step 3: Write the implementation**

In `src/cli/pull/mdh.rs`, replace the body of `fetch_index_set` (keep its doc comment, extend it):

```rust
/// Fetch a collection's regular + search index definitions from the env.
/// Shared by the pull write path and the dry-run index-edit forecast.
///
/// The two listings are independent — spec S6: each REQUIRES its own
/// `collectionName` (422 without it) and there is no bulk form — so awaiting
/// them in sequence paid the sum of two round trips for nothing. `try_join!`
/// makes a dataset cost `max(regular, search)` instead of their sum.
async fn fetch_index_set(
    client: &DataStorageClient,
    collection_name: &str,
    progress: &Arc<Log>,
) -> Result<IndexSet> {
    let (regular, search) = tokio::try_join!(
        async {
            client
                .list_indexes(collection_name, Some(progress.clone()))
                .await
                .with_context(|| format!("listing indexes for '{collection_name}'"))
        },
        async {
            client
                .list_search_indexes(collection_name, Some(progress.clone()))
                .await
                .with_context(|| format!("listing search indexes for '{collection_name}'"))
        },
    )?;
    Ok(IndexSet { regular, search })
}

/// Bound on how many datasets' index fetches are outstanding at once.
///
/// This is NOT the throughput control. The Data Storage token bucket
/// ([`crate::api::rate_limit::RateLimiter::rossum_data_storage`], 30/s) is what
/// actually paces these calls; at the measured 111-372ms per listing a fan-out
/// of 10 attempts 27-90 req/s, which the bucket then meters down to 30. Raising
/// this constant buys nothing and only widens the blast radius of a failure.
pub(crate) const MDH_FANOUT: usize = 10;

/// Fetch index sets for a batch of datasets, concurrently.
///
/// Two levels of overlap, both governed by the Data Storage bucket:
/// `try_join!` WITHIN a dataset (see [`fetch_index_set`]) and
/// `buffer_unordered(MDH_FANOUT)` ACROSS datasets.
///
/// `wanted` is `(dataset_slug, collection_name)` and the returned map is keyed
/// by **dataset slug**, carrying an entry for every requested slug. This helper
/// deliberately does not decide WHICH datasets to fetch — each caller keeps its
/// own scope, so request counts per command stay exactly what they were.
pub(crate) async fn fetch_index_sets(
    client: &DataStorageClient,
    wanted: &[(String, String)],
    progress: &Arc<Log>,
) -> Result<BTreeMap<String, IndexSet>> {
    if wanted.is_empty() {
        return Ok(BTreeMap::new());
    }
    let fetched: Vec<(String, IndexSet)> = futures::stream::iter(wanted.iter().cloned())
        .map(|(slug, name)| {
            let progress = progress.clone();
            async move {
                let set = fetch_index_set(client, &name, &progress).await?;
                Ok::<_, anyhow::Error>((slug, set))
            }
        })
        .buffer_unordered(MDH_FANOUT)
        .try_collect()
        .await?;
    Ok(fetched.into_iter().collect())
}
```

Confirm the file's imports already bring in `futures::stream::{StreamExt, TryStreamExt}` (sub-phase B uses `.map(...).buffer_unordered(...).try_collect()` today) and `std::collections::BTreeMap`; add whichever is missing.

Then replace sub-phase B's inline stream in `process` with a call to the helper. The block from `let client_ref = &client;` down to the `by_slug` binding becomes:

```rust
    // === Sub-phase B: concurrent index fetches per collection (regular +
    //            search), via the shared helper the dry-run forecast also
    //            uses — so the preview can never schedule differently from
    //            the run it previews.
    let total = dataset_dirs.len();
    if total == 0 {
        return Ok((0, conflicts));
    }
    let wanted: Vec<(String, String)> = dataset_dirs
        .iter()
        .map(|(slug, _, c)| (slug.clone(), c.name.clone()))
        .collect();
    let by_slug = fetch_index_sets(&client, &wanted, progress).await?;
    progress.event(Action::Pull, &format!("mdh_indexes ({total} fetched)"));
```

Sub-phase C's `by_slug.get(slug)` is unchanged — it now reads a `BTreeMap` instead of a `HashMap`, which `.get` handles identically.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --locked --lib pull::mdh::`
Expected: PASS — the two new tests plus every existing `mdh` test (the `process` and `plan_mdh_index_edits` suites must be untouched by this change).

- [ ] **Step 5: Commit**

```bash
git add src/cli/pull/mdh.rs
git commit -m "perf(mdh): join a dataset's two index listings, share one fetcher

The regular and search listings are independent (each needs its own
collectionName; there is no bulk form), so awaiting them in sequence
paid two round trips for nothing. fetch_index_sets adds the across-
dataset fan-out and becomes the single scheduler both the pull write
path and the dry-run forecast will use."
```

---

### Task 4: Compute the dataset slug map once, on `MdhListed`

Spec **D7**. Four sites independently re-derive the same `slugify_unique` walk over `catalog.mdh.collections` (C6), and `plan_mdh_index_edits` already carries a comment warning that a divergence would silently target the wrong collection. Task 7's prefetch has to slug-match `process` exactly, so collapsing these is a **prerequisite**, not a cleanup.

**Files:**
- Modify: `src/cli/pull/mdh.rs` (struct, constructor, accessor, `list`, `plan_mdh`, `plan_mdh_index_edits`, `process`, 11 test constructions)
- Modify: `src/cli/sync/execute.rs` (`slug_to_collection` at ~3814, the `MdhListed` reconstruction at ~4045, the test construction at ~4307)
- Modify: `src/cli/sync/mod.rs` (the test construction at ~1663)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `MdhListed` gains `#[derive(Clone)]` and a public field `pub slugs: Vec<String>` — one slug per entry of `collections`, **same index**, in listing order.
  - `pub fn MdhListed::new(client: DataStorageClient, collections: Vec<Collection>, available: bool) -> Self`
  - `pub fn MdhListed::datasets(&self) -> impl Iterator<Item = (&str, &Collection)>` — `(slug, collection)` in listing order. Tasks 5, 6, 7 and 8 all iterate this.

- [ ] **Step 1: Write the failing test**

Append inside the existing `#[cfg(test)] mod tests` in `src/cli/pull/mdh.rs`:

```rust
    /// Spec D7: the slug for each collection is derived ONCE, at listing time,
    /// with the executor's exact rule (listing order, `slugify_unique` dedup) —
    /// so no consumer can drift and silently target a different collection.
    #[test]
    fn mdh_listed_slugs_are_computed_once_in_listing_order() {
        let client = DataStorageClient::new(
            "https://unused.invalid/svc/data-storage/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        // The first two names slugify to the same base: the SECOND must take
        // the deduped slug, and only because it is second in the listing.
        let listed = MdhListed::new(
            client,
            vec![
                Collection { name: "GL Codes".to_string(), extra: Default::default() },
                Collection { name: "gl codes".to_string(), extra: Default::default() },
                Collection { name: "vendors".to_string(), extra: Default::default() },
            ],
            true,
        );
        assert_eq!(listed.slugs, vec!["gl-codes", "gl-codes-2", "vendors"]);
        let pairs: Vec<(&str, &str)> = listed
            .datasets()
            .map(|(slug, c)| (slug, c.name.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("gl-codes", "GL Codes"),
                ("gl-codes-2", "gl codes"),
                ("vendors", "vendors"),
            ],
            "datasets() must pair each slug with ITS collection, positionally"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --locked --lib pull::mdh::tests::mdh_listed_slugs_are_computed_once_in_listing_order`
Expected: FAIL to compile — "no function or associated item named `new` found for struct `MdhListed`".

- [ ] **Step 3: Write the implementation**

In `src/cli/pull/mdh.rs`, replace the `MdhListed` declaration (keeping its existing doc comment on `client`, `collections` and `available`) with:

```rust
/// Opaque listed state for MDH — the client handle plus the collection list.
/// We carry the client here because it's constructed from env_cfg + token,
/// which live in `run_drivers` scope.
#[derive(Clone)]
pub struct MdhListed {
    pub client: DataStorageClient,
    pub collections: Vec<Collection>,
    /// Dataset slug for each entry of `collections`, **same index**, derived
    /// once at listing time by [`MdhListed::new`].
    ///
    /// Four call sites used to re-derive this walk independently (the pull
    /// write path, the dry-run structural plan, the dry-run index-edit
    /// forecast and the sync executor's `slug_to_collection`). They must agree
    /// byte-for-byte or a fetch silently targets the wrong collection — so the
    /// walk happens exactly once and everyone reads the result.
    pub slugs: Vec<String>,
    /// Whether MDH is provisioned on this env. `true` when the collection
    /// listing succeeded (even with zero collections); `false` when the
    /// Data Storage endpoint 404s (MDH not enabled on the cluster). A 404
    /// and a genuinely-empty listing both yield `collections == []`, so this
    /// flag is the only way to tell them apart — the deploy uses it to gate
    /// collection creation (create on a fresh-but-enabled env; never attempt
    /// it against a cluster without MDH).
    pub available: bool,
}

impl MdhListed {
    /// Build from a listing, deriving each collection's dataset slug once.
    pub fn new(client: DataStorageClient, collections: Vec<Collection>, available: bool) -> Self {
        let mut used: HashSet<String> = HashSet::new();
        let slugs = collections
            .iter()
            .map(|c| {
                let slug = slugify_unique(&c.name, &used);
                used.insert(slug.clone());
                slug
            })
            .collect();
        Self { client, collections, slugs, available }
    }

    /// `(dataset_slug, collection)` in listing order.
    pub fn datasets(&self) -> impl Iterator<Item = (&str, &Collection)> {
        self.slugs
            .iter()
            .map(String::as_str)
            .zip(self.collections.iter())
    }
}
```

Rewrite `list`'s tail to use it:

```rust
    Ok(MdhListed::new(client, collections, available))
```

Replace each of the three in-file slug walks with `datasets()`:

- `plan_mdh` (~line 183): delete the `let mut used` / `for c in &listed.collections` slug derivation and iterate `for (slug, _c) in listed.datasets()` instead, inserting `slug.to_string()` into `remote_slugs`.
- `plan_mdh_index_edits` (~line 381): delete the `let mut used` / `slugify_unique` lines and change the loop header to `for (slug, c) in listed.datasets()`, replacing `&slug` with `slug` at its uses.
- `process` (~line 750): destructure `let MdhListed { client, collections, slugs, available: _ } = listed;`, drop the `let mut used` line, and drive sub-phase A's loop with `for (slug, c) in slugs.into_iter().zip(collections)` (it consumes both, matching today's `for c in collections`).

In `src/cli/sync/execute.rs`, replace the `slug_to_collection` walk:

```rust
            let mut slug_to_collection: BTreeMap<String, &crate::model::Collection> =
                BTreeMap::new();
            for (slug, c) in catalog.mdh.datasets() {
                slug_to_collection.insert(slug.to_string(), c);
            }
```

(the `use std::collections::HashSet as HashSetForSlugs;` line and the `used` binding go away), and replace the stage-3 reconstruction:

```rust
            let listed = catalog.mdh.clone();
```

In the three test constructions (`src/cli/sync/execute.rs` ~4307, `src/cli/sync/mod.rs` ~1663, and each of the 11 in `src/cli/pull/mdh.rs`'s test module), replace the struct literal with the constructor. For example the two placeholder ones become:

```rust
            mdh: crate::cli::pull::mdh::MdhListed::new(
                crate::api::data_storage::DataStorageClient::new(
                    "https://unused.invalid/svc/data-storage/api/v1".to_string(),
                    "TEST".to_string(),
                )
                .unwrap(),
                vec![],
                false,
            ),
```

and the mdh test-module ones follow the same shape with their own `collections` vec and `available` flag.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --locked --lib pull::mdh::`
Expected: PASS, including every pre-existing `plan_mdh*` and `process` test — they exercise the slug derivation this task centralised, so an off-by-one in `datasets()` shows up here.

Run: `cargo test -p rdc --locked --lib sync::`
Expected: PASS.

- [ ] **Step 5: Verify the duplicates are gone**

Run: `grep -rn "slugify_unique" src/cli/pull/mdh.rs src/cli/sync/execute.rs | grep -v "fn new"`
Expected: exactly one hit — the call inside `MdhListed::new`. (The `execute.rs` hits at lines ~151-305 and ~2387-2526 are other kinds' slug walks, not MDH; they must NOT appear in this grep because the pattern is scoped, but if they do, confirm by line number that none of them iterates `mdh.collections`.)

- [ ] **Step 6: Commit**

```bash
git add src/cli/pull/mdh.rs src/cli/sync/execute.rs src/cli/sync/mod.rs
git commit -m "refactor(mdh): derive each dataset slug once, on MdhListed

Four sites re-walked the same slugify_unique sequence over the
collection listing; a divergence between any two would silently target
the wrong collection, which one of them already carried a comment
warning about. MdhListed::new derives them; datasets() hands out
(slug, collection) pairs."
```

---

### Task 5: The dry-run index forecast stops fetching one dataset at a time

Spec **D4**, and the single largest measured defect in the whole design. `plan_mdh_index_edits` is a plain sequential `for` loop (C3): B9 measured 42 requests in **6.75s at max concurrency 3** — 79% of that run's wall clock — which is why `sync --dry-run` takes **2.1× the wall clock of the real sync it previews** (B2 vs B1).

The scope is unchanged: a dataset with no local `indexes.json` is still skipped (`plan_mdh` stage 3 already forecasts it as "(new)"), so the request count per command is identical. Only the scheduling changes.

**Files:**
- Modify: `src/cli/pull/mdh.rs` (`plan_mdh_index_edits`)

**Interfaces:**
- Consumes: Task 3's `fetch_index_sets`, Task 4's `MdhListed::datasets`.
- Produces: no signature change — `plan_mdh_index_edits(&MdhListed, &Lockfile, &Paths, &Arc<Log>) -> Result<Vec<MdhPlanItem>>` is untouched, and the returned items keep their existing order (dataset listing order).

- [ ] **Step 1: Write the failing test**

Append inside the existing `#[cfg(test)] mod tests` in `src/cli/pull/mdh.rs`:

```rust
    /// Spec D4/B9: the dry-run index forecast must fan out across datasets
    /// instead of walking them one round trip at a time — while fetching the
    /// SAME set of datasets it always did (the ones with a local
    /// `indexes.json`) and returning items in listing order.
    ///
    /// Three datasets have a local file and one does not. With every listing
    /// delayed 200ms, sequential costs >= 6 x 200ms; concurrent costs ~200ms.
    #[tokio::test(flavor = "multi_thread")]
    async fn plan_mdh_index_edits_fans_out_across_datasets() {
        use crate::state::{Lockfile, ObjectEntry, content_hash};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let delayed = |body: serde_json::Value| {
            ResponseTemplate::new(200)
                .set_body_json(body)
                .set_delay(std::time::Duration::from_millis(200))
        };
        // Env advertises index "acct"; the local snapshots below say "acct_v2".
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(delayed(serde_json::json!({
                "code": "ok",
                "result": [ { "name": "acct", "key": { "accountName": 1 } } ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(delayed(serde_json::json!({ "code": "ok", "result": [] })))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let local = b"{\n  \"regular\": [\n    {\n      \"key\": {\n        \"accountName\": 1\n      },\n      \"name\": \"acct_v2\"\n    }\n  ],\n  \"search\": []\n}\n";
        let mut lockfile = Lockfile::default();
        let mut mdh = std::collections::BTreeMap::new();
        for slug in ["a-set", "b-set", "c-set"] {
            let dir = paths.dataset_dir(slug);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("indexes.json"), local).unwrap();
            mdh.insert(
                slug.to_string(),
                ObjectEntry {
                    id: 0,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some(content_hash(local, &Lockfile::default())),
                    secrets_hash: None,
                },
            );
        }
        lockfile.objects.insert("mdh_indexes".to_string(), mdh);

        let mk = |name: &str| Collection { name: name.to_string(), extra: Default::default() };
        let listed = MdhListed::new(
            DataStorageClient::new(server.uri(), "TEST".to_string()).unwrap(),
            // "d-set" has NO local dataset dir: it must not be fetched.
            vec![mk("a-set"), mk("b-set"), mk("c-set"), mk("d-set")],
            true,
        );
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);

        let start = std::time::Instant::now();
        let items = plan_mdh_index_edits(&listed, &lockfile, &paths, &progress)
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(
            items.iter().map(|i| i.line.clone()).collect::<Vec<_>>(),
            vec![
                "mdh/a-set (index update)".to_string(),
                "mdh/b-set (index update)".to_string(),
                "mdh/c-set (index update)".to_string(),
            ],
            "items must stay in dataset listing order, and d-set must be absent"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            6,
            "scope is unchanged: 3 datasets with a local file x 2 calls; the \
             one without a local dir is still never fetched"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "3 datasets must overlap; sequential would be >= 1.2s, took {elapsed:?}",
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --locked --lib pull::mdh::tests::plan_mdh_index_edits_fans_out_across_datasets`
Expected: FAIL on the elapsed assertion — the sequential loop takes ~1.2s.

- [ ] **Step 3: Write the implementation**

In `src/cli/pull/mdh.rs`, restructure `plan_mdh_index_edits` into an offline scoping pass, one batched fetch, and a sequential compare pass. After Task 4 the function opens with `let mut items = Vec::new();`, the `available` guard, and a `for (slug, c) in listed.datasets()` loop whose first half fetches and compares the index set and whose second half is the row-data forecast. Replace everything from `let mut items` through that loop's **index half** with:

```rust
    let mut items = Vec::new();
    if !listed.available {
        return Ok(items);
    }

    // Offline pass: decide WHICH datasets need a fetch. A collection with no
    // local `indexes.json` is territory `plan_mdh` stage 3 already forecasts
    // as "(new)" — fetching it would have nothing to compare against and
    // would double-report. Scope is therefore identical to the sequential
    // version; only the scheduling below changes.
    let wanted: Vec<(String, String)> = listed
        .datasets()
        .filter(|(slug, _)| paths.dataset_dir(slug).join("indexes.json").is_file())
        .map(|(slug, c)| (slug.to_string(), c.name.clone()))
        .collect();

    // One batched fetch, shared with the real pull's sub-phase B so the
    // preview can never schedule differently from the run it previews.
    let sets = fetch_index_sets(&listed.client, &wanted, progress).await?;

    // Sequential compare pass, in dataset listing order.
    for (slug, name) in &wanted {
        let ix_path = paths.dataset_dir(slug).join("indexes.json");
        let set = sets
            .get(slug)
            .expect("fetch_index_sets returns an entry for every requested slug");
        let proposed = proposed_index_bytes(set)?;
        let base = lockfile
            .objects
            .get("mdh_indexes")
            .and_then(|m| m.get(slug))
            .and_then(|e| e.content_hash.clone());
        if let Some(item) = index_edit_item(slug, &ix_path, base.as_deref(), &proposed)? {
            items.push(item);
        }

        // Row-data forecast for manual datasets — the row-level analogue of
        // the index-edit forecast above. `?`, not `unwrap_or`: this function
        // is fallible and runs in the same dry-run pass as a real sync, so a
        // malformed "data" flag must fail the preview exactly like it fails
        // the real run (see the doc comment on this function).
        if read_data_mode(&paths.dataset_dir(slug))? == DataMode::Manual {
            let data_path = paths.dataset_data(slug);
            if data_path.is_file() {
                let rows = listed.client.find_all(name, Some(progress.clone())).await?;
                // ... existing row-forecast body, unchanged, with `&c.name`
                // replaced by `name` and `&slug` replaced by `slug`
            }
        }
    }
```

Keep the rest of the row-forecast body exactly as it is. Note the two substitutions the loop-variable change forces: `&c.name` becomes `name`, and `&slug` / `slug.clone()` become `slug` / `slug.clone()` against a `&String`.

**Do not** move the row fetch into the batch here — that is Task 8, which changes both the preview and the real pull together.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --locked --lib pull::mdh::`
Expected: PASS — the new test plus the three pre-existing `plan_mdh_index_edits_*` tests, which pin the forecast's *content* and must be unaffected.

- [ ] **Step 5: Commit**

```bash
git add src/cli/pull/mdh.rs
git commit -m "perf(mdh): batch the dry-run index forecast instead of walking it

The forecast loop fetched one dataset at a time -- 42 requests in 6.75s
at max concurrency 3 in the traced run, 79% of its wall clock, which is
why --dry-run cost 2.1x the sync it previews. Same datasets, same
request count, one batched fetch."
```

---
### Task 6: MDH listing leaves the core list stream

Spec **D5**. MDH's `collections/list` is the 13th arm of the same `buffer_unordered(PULL_FANOUT)` stream as the 12 core list kinds (C4), so it cannot even be dispatched until two waves of core lists have drained — the traced run saw it start at **t = 1.05s**, queued behind 11 core lists. The two services throttle independently (S5) and now have independent buckets (Task 2), so there is no reason for one to wait on the other's slot.

On its own this task is worth a slot, not a second. It is here because it is the **enabler for Task 7**: once listing also prefetches index sets, the MDH arm becomes long, and a long arm holding one of five core slots would be actively harmful. Land the structure first, then the payload.

**Files:**
- Modify: `src/cli/pull/common.rs` (`list_remote`)
- Modify: `tests/cli_sync.rs` (new test)

**Interfaces:**
- Consumes: Task 4's `MdhListed`.
- Produces: no signature change. `list_remote(&mut PullCtx, &EnvConfig, &str, &str, &Arc<Log>) -> Result<RemoteCatalog>` is untouched, and `RemoteCatalog.mdh` is populated exactly as before.

- [ ] **Step 1: Write the failing test**

Append to `tests/cli_sync.rs`:

```rust
/// Spec D5: the MDH listing must be dispatched as a SIBLING of the core list
/// stream, not as its 13th arm.
///
/// Every core list endpoint is delayed 200ms; the Data Storage listing is not.
/// As a sibling, the Data Storage request goes out in the very first wave, so
/// it lands among the first handful of requests the server sees. As the 13th
/// arm of a `buffer_unordered(5)` stream it could not start until two waves of
/// core lists had completed, putting it eleventh or later.
///
/// This asserts arrival ORDER, which `received_requests()` preserves, rather
/// than a wall-clock threshold — the scheduling is the thing under test.
#[tokio::test]
async fn mdh_listing_is_dispatched_alongside_the_core_list_stream() {
    let server = MockServer::start().await;
    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    let slow = |body: serde_json::Value| {
        ResponseTemplate::new(200)
            .set_body_json(body)
            .set_delay(std::time::Duration::from_millis(200))
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(slow(fixture("organization.json")))
        .mount(&server)
        .await;
    for ep in [
        "/api/v1/workspaces",
        "/api/v1/queues",
        "/api/v1/inboxes",
        "/api/v1/hooks",
        "/api/v1/rules",
        "/api/v1/labels",
        "/api/v1/engines",
        "/api/v1/engine_fields",
        "/api/v1/workflows",
        "/api/v1/workflow_steps",
        "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(slow(empty.clone()))
            .mount(&server)
            .await;
    }
    // Data Storage, on the same host: `derive_data_storage_base` turns
    // `<uri>/api/v1` into `<uri>/svc/data-storage/api`. Answer instantly.
    Mock::given(method("POST"))
        .and(path("/svc/data-storage/api/v1/collections/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
        )
        .mount(&server)
        .await;

    let project = TempDir::new().unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert()
        .success();

    let requests = server.received_requests().await.unwrap();
    let ds_index = requests
        .iter()
        .position(|r| r.url.path() == "/svc/data-storage/api/v1/collections/list")
        .expect("the Data Storage listing must happen");
    assert!(
        ds_index < 6,
        "MDH listing must go out in the first wave, not queued behind the core \
         list stream; it arrived at position {ds_index} of {}",
        requests.len(),
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --locked --test cli_sync mdh_listing_is_dispatched_alongside_the_core_list_stream`
Expected: FAIL — "it arrived at position 12 of …" (or similar; anything ≥ 6).

- [ ] **Step 3: Write the implementation**

In `src/cli/pull/common.rs`:

1. Delete `Mdh` from the `enum Kind`, from the `kinds` array, from `enum Listed`, and delete the whole `Kind::Mdh => { … }` match arm.
2. Replace the `let results: Vec<Listed> = futures::stream::iter(...)…try_collect().await?;` binding and the following `progress.end_phase();` with:

```rust
    progress.start_phase(Action::List, "listing", 0);
    // The core kinds share one bounded stream against the core API's 10 req/s
    // bucket. MDH talks to a DIFFERENT service with its OWN bucket (spec S5:
    // the two throttle independently on the same token), so it runs as a
    // sibling rather than competing for a core slot — and, from the task that
    // adds the index-set prefetch, so the whole MDH phase overlaps core
    // listing instead of following it.
    let core = futures::stream::iter(kinds.iter().copied())
        .map(|kind| {
            async move {
                // ... existing future body, unchanged ...
            }
        })
        .buffer_unordered(PULL_FANOUT)
        .try_collect::<Vec<Listed>>();

    let mdh_arm = async {
        let r = crate::cli::pull::mdh::list(env_cfg, token, progress)
            .await
            .with_context(|| format!("listing MDH datasets for env '{env}'"))?;
        progress.event(
            Action::List,
            &format!("mdh_datasets ({})", r.collections.len()),
        );
        anyhow::Ok(r)
    };

    let (results, mdh) = tokio::try_join!(core, mdh_arm)?;
    progress.end_phase();
```

3. Delete `let mut mdh: Option<crate::cli::pull::mdh::MdhListed> = None;`, the `Listed::Mdh(v) => mdh = Some(v),` match arm, and `let mdh = mdh.expect("mdh listed");`. The `mdh` binding now comes from the `try_join!`.

Leave every other binding, the `Ok(RemoteCatalog { … })` construction, and `prefetch_queue_schemas` exactly as they are.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --locked --test cli_sync mdh_listing_is_dispatched_alongside_the_core_list_stream`
Expected: PASS.

Run: `cargo test -p rdc --locked --test cli_sync mdh`
Expected: PASS — every existing MDH-through-sync test.

- [ ] **Step 5: Commit**

```bash
git add src/cli/pull/common.rs tests/cli_sync.rs
git commit -m "perf(pull): list MDH as a sibling of the core list stream

MDH's collections/list was the 13th arm of the core buffer_unordered,
so it could not start until two waves of core lists drained -- t=1.05s
in the traced run. Different service, independent bucket, no reason to
queue behind a core slot."
```

---

### Task 7: Prefetch index sets during listing

Spec **D6**. With Task 6 in place the MDH arm is free-running, so it can do its whole phase there: list the collections, then immediately fetch the index sets, while the core stream is still working. On a steady-state sync that moves the entire MDH read phase off the critical path.

**Correction to spec D6 — the prefetch is gated on the cycle being read-only.** D6 as written is unsound for `process`, which is stage 3 of the MDH cycle: the pull-back that runs *after* stages 1 and 2 push index changes. Seeding it from a listing-time snapshot would write `indexes.json` from **pre-push** state, so a just-created index would be silently absent from the snapshot and the next cycle would try to create it again — the period-2 churn this codebase has fought before. The pre-existing test `sync_mdh_index_create_counts_as_changed_when_materialized` detects exactly this.

So `mdh::list` prefetches **only when the cycle performs no writes** (`dry_run || no_push`). When the cycle writes, `index_sets` stays empty and `process`'s "fetch the remainder" logic naturally fetches its whole subset fresh — no second code path. Request counts stay identical in all four cases, and the pull-back always sees post-push state.

**Scope refinement vs. the spec.** D6 scopes the prefetch to "collections whose local dataset dir exists". Use the tighter predicate **"whose local `indexes.json` exists"** instead. Both are decidable offline, both are a superset of nothing the run doesn't already need, but the tighter one makes the dry-run request count *exactly* unchanged rather than "unchanged or higher": a dataset dir with no `indexes.json` (a hand-made dir, or one whose file was deleted) is territory the dry-run forecast deliberately skips, and prefetching it would add a request the preview never used to make. `process` still fetches the remainder, so the real sync's total is unchanged either way.

| case | prefetched | fetched in `process` | total vs today |
|---|---|---|---|
| steady `--no-push` (read-only) | datasets with `indexes.json` | the remainder, ~0 | same, now overlapped |
| steady `--dry-run` (read-only) | datasets with `indexes.json` | `process` is never reached | same, now overlapped |
| a sync that writes | **0 — prefetch gated off** | all of its subset, fresh | same, and correct |
| first full pull (no local files) | 0 | all | identical |
| new remote collection | 0 for it | its 2 calls | identical |

**Files:**
- Modify: `src/cli/pull/mdh.rs` (`MdhListed`, `list`, `process` sub-phase B, `plan_mdh_index_edits`)
- Modify: `src/cli/pull/common.rs` (pass `paths` into `mdh::list`)

**Interfaces:**
- Consumes: Task 3's `fetch_index_sets`, Task 4's `MdhListed::datasets`, Task 6's free-running MDH arm.
- Produces:
  - `MdhListed` gains `pub index_sets: BTreeMap<String, IndexSet>` — index sets already fetched at listing time, keyed by dataset slug. Empty on a fresh tree. `MdhListed::new` initialises it empty; the field is filled by `list`.
  - `mdh::list` signature changes to `pub async fn list(env_cfg: &EnvConfig, token: &str, prefetch_for: Option<&crate::paths::Paths>, progress: &Arc<Log>) -> Result<MdhListed>` — `Some(paths)` prefetches, `None` does not. Its only caller is `list_remote`.
  - `list_remote` gains a `prefetch_mdh_indexes: bool` parameter, passed through as `Some(ctx_ref.paths)` / `None`. Its only caller is `crate::cli::sync::run`, which passes `dry_run || no_push`.

- [ ] **Step 1: Write the failing test**

Append inside the existing `#[cfg(test)] mod tests` in `src/cli/pull/mdh.rs`:

```rust
    /// Spec D6: listing prefetches the index sets of datasets that already have
    /// a local `indexes.json`, so a steady-state sync's whole MDH read phase
    /// overlaps core listing. A collection with no local file is NOT
    /// prefetched — it is a new dataset, and `process` fetches it there.
    #[tokio::test(flavor = "multi_thread")]
    async fn list_prefetches_index_sets_for_datasets_with_a_local_file() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // `list` builds its own client from `env_cfg.data_storage_base()`, which
        // turns `<uri>/api/v1` into `<uri>/svc/data-storage/api` — so the mocks
        // must sit under that prefix, unlike the tests that hand a client a bare
        // `server.uri()`.
        let ds = "/svc/data-storage/api/v1";
        Mock::given(method("POST"))
            .and(path(format!("{ds}/collections/list")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [ { "name": "gl-codes" }, { "name": "vendors" } ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{ds}/indexes/list")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": [ { "name": "acct" } ] }),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{ds}/search_indexes/list")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        // Only `gl-codes` has a local snapshot.
        let dir = paths.dataset_dir("gl-codes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("indexes.json"), b"{\n  \"regular\": [],\n  \"search\": []\n}\n").unwrap();

        let env_cfg = crate::config::EnvConfig {
            api_base: format!("{}/api/v1", server.uri()),
            org_id: 1,
        };
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let listed = list(&env_cfg, "TEST", &paths, &progress).await.unwrap();

        assert!(listed.available);
        assert_eq!(listed.slugs, vec!["gl-codes", "vendors"]);
        assert_eq!(
            listed.index_sets.keys().collect::<Vec<_>>(),
            vec!["gl-codes"],
            "only the dataset with a local indexes.json is prefetched"
        );
        assert_eq!(listed.index_sets["gl-codes"].regular.len(), 1);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            3,
            "1 collections/list + 2 index calls for the one local dataset"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --locked --lib pull::mdh::tests::list_prefetches_index_sets_for_datasets_with_a_local_file`
Expected: FAIL to compile — `list` takes 3 arguments, and `MdhListed` has no field `index_sets`.

- [ ] **Step 3: Write the implementation**

In `src/cli/pull/mdh.rs`:

1. Add the field to `MdhListed`, after `slugs`:

```rust
    /// Index sets already fetched at listing time, keyed by dataset slug.
    ///
    /// Scoped to datasets that already have a local `indexes.json`, which is
    /// decidable offline and sits exactly between the two consumers' needs:
    /// the dry-run forecast wants precisely this set, and a real pull wants
    /// this set plus any brand-new collection (which `process` fetches as the
    /// remainder). So neither command's request count changes — they just
    /// happen earlier, overlapped with core listing. Empty on a fresh tree.
    pub index_sets: BTreeMap<String, IndexSet>,
```

and initialise it in `MdhListed::new` with `index_sets: BTreeMap::new(),`.

2. Give `list` the `paths` parameter and the prefetch:

```rust
pub async fn list(
    env_cfg: &EnvConfig,
    token: &str,
    paths: &crate::paths::Paths,
    progress: &Arc<Log>,
) -> Result<MdhListed> {
    // ... existing client construction and `list_collections` match, unchanged ...

    let mut listed = MdhListed::new(client, collections, available);

    // Prefetch this env's index sets while the core list stream is still
    // running. Scope: datasets that already have a local `indexes.json`. See
    // the field doc on `MdhListed::index_sets` for why that predicate, and
    // why it leaves every command's request count unchanged.
    let wanted: Vec<(String, String)> = listed
        .datasets()
        .filter(|(slug, _)| paths.dataset_dir(slug).join("indexes.json").is_file())
        .map(|(slug, c)| (slug.to_string(), c.name.clone()))
        .collect();
    listed.index_sets = fetch_index_sets(&listed.client, &wanted, progress).await?;

    Ok(listed)
}
```

3. In `process`, take `index_sets` out of the destructure and seed sub-phase B with it:

```rust
    let MdhListed {
        client,
        collections,
        slugs,
        index_sets,
        available: _,
    } = listed;
```

and sub-phase B becomes:

```rust
    // Datasets whose index set listing already prefetched are free; fetch only
    // the remainder (normally just brand-new collections).
    let wanted: Vec<(String, String)> = dataset_dirs
        .iter()
        .filter(|(slug, _, _)| !index_sets.contains_key(slug))
        .map(|(slug, _, c)| (slug.clone(), c.name.clone()))
        .collect();
    let mut by_slug = index_sets;
    by_slug.extend(fetch_index_sets(&client, &wanted, progress).await?);
    progress.event(Action::Pull, &format!("mdh_indexes ({total} fetched)"));
```

(`total` stays `dataset_dirs.len()` — the summary line reports datasets processed, not requests made, exactly as before.)

4. In `plan_mdh_index_edits`, read the prefetch first and fetch only what is missing. Replace the `let sets = fetch_index_sets(...)` line from Task 5 with:

```rust
    // Listing already prefetched exactly this scope; `missing` is empty in the
    // steady state and non-empty only if a dataset's local file appeared
    // between listing and now.
    let missing: Vec<(String, String)> = wanted
        .iter()
        .filter(|(slug, _)| !listed.index_sets.contains_key(slug))
        .cloned()
        .collect();
    let fetched = fetch_index_sets(&listed.client, &missing, progress).await?;
```

and change the per-dataset lookup inside the compare loop to:

```rust
        let set = listed
            .index_sets
            .get(slug)
            .or_else(|| fetched.get(slug))
            .expect("every wanted slug is either prefetched or fetched above");
```

In `src/cli/pull/common.rs`, pass the paths through the MDH arm:

```rust
        let r = crate::cli::pull::mdh::list(env_cfg, token, ctx_ref.paths, progress)
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --locked --lib pull::mdh::`
Expected: PASS. Existing `plan_mdh_index_edits_*` tests construct `MdhListed` with an empty `index_sets`, so they exercise the fetch-the-missing fallback — they must stay green unchanged.

Run: `cargo test -p rdc --locked --test cli_sync mdh`
Expected: PASS.

- [ ] **Step 5: Verify request counts really are unchanged**

Run: `cargo test -p rdc --locked --test cli_sync`
Expected: PASS. Any test that counts MDH requests through a full sync or dry-run is the real gate here — this task's whole contract is "same requests, earlier".

- [ ] **Step 6: Commit**

```bash
git add src/cli/pull/mdh.rs src/cli/pull/common.rs
git commit -m "perf(mdh): prefetch index sets during listing

The MDH arm now runs its whole read phase while the core list stream is
still working. Scoped to datasets with a local indexes.json, which is
what the dry-run forecast needs exactly and what a real pull needs
minus brand-new collections, so no command's request count changes."
```

---

### Task 8: Fan out MDH row pulls across datasets

Spec **D8**. `pull_dataset_data` runs sequentially inside sub-phase C's apply loop, so N manual datasets cost N × (count + find) round trips in series. Split it the same way as the index fetches: concurrent across datasets, sequential apply.

**The `count_documents` guardrail stays inside each dataset's future.** It is what stops a mis-flagged import-fed collection from being dragged into the snapshot before it is read, so `count` must still gate `find_all` *within* a dataset. Only the across-dataset scheduling changes.

**Reported as projected, not verified (spec R6).** The measurement org has no manual datasets, so unlike Tasks 3–7 this one is designed by analogy to D4 rather than from a trace. Task 13 must say so.

**Deliberately out of scope: the dry-run row forecast.** `plan_mdh_index_edits`'s row leg calls `find_all` with no count guardrail; giving it the same treatment would mean either adding a count request per dataset (forbidden — request counts must not change) or a second, subtly different helper. It stays sequential and Task 13 reports it as a known remaining gap.

**Files:**
- Modify: `src/cli/pull/mdh.rs` (split `pull_dataset_data`, add `fetch_dataset_rows`, rewire sub-phase C)

**Interfaces:**
- Consumes: Task 3's `MDH_FANOUT`, Task 4's `MdhListed`.
- Produces:
  - `pub(crate) async fn fetch_dataset_rows(client: &DataStorageClient, paths: &Paths, wanted: &[(String, String)], progress: &Arc<Log>) -> Result<BTreeMap<String, (usize, Vec<Value>)>>` — keyed by dataset slug, value is `(row_count, rows)`.
  - `pub(crate) async fn apply_dataset_rows(ctx: &mut PullCtx<'_>, slug: &str, count: usize, rows: Vec<Value>, progress: &Arc<Log>) -> Result<(bool, usize)>` — the offline/FS/lockfile half.
  - `pull_dataset_data` **keeps its exact current signature** as a thin `fetch` + `apply` wrapper, so its three existing tests are untouched.

- [ ] **Step 1: Write the failing test**

Append inside the existing `#[cfg(test)] mod tests` in `src/cli/pull/mdh.rs`:

```rust
    /// Spec D8: rows for several manual datasets are fetched concurrently, but
    /// the `$count` guardrail still precedes that dataset's `find` — that
    /// ordering is what stops an oversized collection being read at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_dataset_rows_fans_out_but_counts_before_finding() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let slow = |body: serde_json::Value| {
            ResponseTemplate::new(200)
                .set_body_json(body)
                .set_delay(std::time::Duration::from_millis(200))
        };
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(slow(
                serde_json::json!({ "code": "ok", "result": [ { "n": 2 } ] }),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .and(body_partial_json(serde_json::json!({ "collectionName": "GL_CODES" })))
            .respond_with(slow(serde_json::json!({
                "code": "ok", "result": [ { "code": "1000" }, { "code": "2000" } ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .and(body_partial_json(serde_json::json!({ "collectionName": "VENDORS" })))
            .respond_with(slow(
                serde_json::json!({ "code": "ok", "result": [ { "name": "acme" } ] }),
            ))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let client = DataStorageClient::new(server.uri(), "TEST".to_string()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let wanted = vec![
            ("gl-codes".to_string(), "GL_CODES".to_string()),
            ("vendors".to_string(), "VENDORS".to_string()),
        ];

        let start = std::time::Instant::now();
        let rows = fetch_dataset_rows(&client, &paths, &wanted, &progress)
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(rows["gl-codes"].0, 2, "count is carried back for the warn");
        assert_eq!(rows["gl-codes"].1.len(), 2);
        assert_eq!(rows["vendors"].1.len(), 1);
        assert!(
            elapsed < std::time::Duration::from_millis(700),
            "the two datasets must overlap; sequential would be ~800ms, took {elapsed:?}",
        );

        // Within a dataset, its $count must precede its find.
        let requests = server.received_requests().await.unwrap();
        let pos = |p: &str, coll: &str| {
            requests
                .iter()
                .position(|r| {
                    r.url.path() == p
                        && String::from_utf8_lossy(&r.body).contains(coll)
                })
                .unwrap_or_else(|| panic!("no {p} for {coll}"))
        };
        assert!(
            pos("/v1/data/aggregate", "GL_CODES") < pos("/v1/data/find", "GL_CODES"),
            "the size guardrail must still gate the read"
        );
        assert!(
            pos("/v1/data/aggregate", "VENDORS") < pos("/v1/data/find", "VENDORS"),
            "the size guardrail must still gate the read"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --locked --lib pull::mdh::tests::fetch_dataset_rows_fans_out_but_counts_before_finding`
Expected: FAIL to compile — "cannot find function `fetch_dataset_rows`".

- [ ] **Step 3: Write the implementation**

In `src/cli/pull/mdh.rs`, split `pull_dataset_data`. Everything from `let count = client.count_documents(...)` down to and including the `find_all` call becomes `fetch_dataset_rows`'s per-dataset body; everything from the `__digest_md5` warn to the end becomes `apply_dataset_rows`.

```rust
/// Fetch the rows of several manual datasets concurrently.
///
/// `wanted` is `(dataset_slug, collection_name)`; the map is keyed by slug and
/// carries `(row_count, rows)` — the count travels back so the
/// `ROW_WARN_THRESHOLD` warning can be emitted on the sequential apply path,
/// keeping progress output in slug order rather than completion order.
///
/// The `$count` guardrail stays INSIDE each dataset's future: refusing an
/// oversized collection BEFORE reading it is the whole point of it, so that
/// ordering is load-bearing and only the across-dataset scheduling changes.
pub(crate) async fn fetch_dataset_rows(
    client: &DataStorageClient,
    paths: &crate::paths::Paths,
    wanted: &[(String, String)],
    progress: &Arc<Log>,
) -> Result<BTreeMap<String, (usize, Vec<serde_json::Value>)>> {
    use crate::snapshot::mdh_data::ROW_HARD_LIMIT;

    if wanted.is_empty() {
        return Ok(BTreeMap::new());
    }
    let fetched: Vec<(String, (usize, Vec<serde_json::Value>))> =
        futures::stream::iter(wanted.iter().cloned())
            .map(|(slug, name)| {
                let progress = progress.clone();
                async move {
                    let count = client
                        .count_documents(&name, Some(progress.clone()))
                        .await
                        .with_context(|| format!("counting rows of '{name}'"))?;
                    if count > ROW_HARD_LIMIT {
                        anyhow::bail!(
                            "mdh/{slug}: '{name}' holds {count} rows, over rdc's \
                             {ROW_HARD_LIMIT}-row ceiling for versioned MDH data. Remove the \
                             \"data\" key from {}/{COLLECTION_MANIFEST} to stop versioning \
                             this dataset's rows (its name and indexes stay managed).",
                            paths.dataset_dir(&slug).display(),
                        );
                    }
                    let rows = client
                        .find_all(&name, Some(progress.clone()))
                        .await
                        .with_context(|| format!("reading rows of '{name}'"))?;
                    Ok::<_, anyhow::Error>((slug, (count, rows)))
                }
            })
            .buffer_unordered(MDH_FANOUT)
            .try_collect()
            .await?;
    Ok(fetched.into_iter().collect())
}

/// The offline half of a manual dataset's row pull: warn, decide, write,
/// record. Sequential by construction — it mutates `ctx.lockfile` and the
/// working tree, and its progress lines must land in slug order.
pub(crate) async fn apply_dataset_rows(
    ctx: &mut PullCtx<'_>,
    slug: &str,
    count: usize,
    rows: Vec<serde_json::Value>,
    progress: &Arc<Log>,
) -> Result<(bool, usize)> {
    use crate::snapshot::mdh_data::{ROW_WARN_THRESHOLD, to_jsonl};

    if count > ROW_WARN_THRESHOLD {
        progress.event(
            Action::Warn,
            &format!(
                "mdh/{slug}: {count} rows is large for a git-versioned dataset \
                 (warns above {ROW_WARN_THRESHOLD})"
            ),
        );
    }
    // ... the existing body from the `__digest_md5` warn through the final
    // `Ok((matches!(action, ...), conflicts))`, unchanged, with
    // `collection_name` no longer in scope (it is not used past this point).
}

/// Pull one manual dataset's rows into `data.jsonl`.
///
/// Returns `(changed, conflicts)`. Costs two calls (`$count` for the guardrail,
/// then one `find`) and is invoked ONLY for datasets flagged `"data": "manual"`,
/// so a metadata-only dataset stays exactly as cheap as it is today.
pub(crate) async fn pull_dataset_data(
    ctx: &mut PullCtx<'_>,
    client: &DataStorageClient,
    collection_name: &str,
    slug: &str,
    progress: &Arc<Log>,
) -> Result<(bool, usize)> {
    let wanted = [(slug.to_string(), collection_name.to_string())];
    let mut fetched = fetch_dataset_rows(client, ctx.paths, &wanted, progress).await?;
    let (count, rows) = fetched
        .remove(slug)
        .expect("fetch_dataset_rows returns every requested slug");
    apply_dataset_rows(ctx, slug, count, rows, progress).await
}
```

Then rewire sub-phase C in `process`. Before the `for (slug, dataset_dir, c) in &dataset_dirs` loop, add the batched row fetch:

```rust
    // Row data for datasets that opted in. `read_data_mode` is offline, so
    // deciding the batch costs nothing — but note the `.ok()`: a MALFORMED
    // `"data"` flag must still surface from the sequential loop below, at the
    // same point it does today, rather than aborting before anything is
    // written. A dataset whose flag will not parse is simply not prefetched.
    let manual: Vec<(String, String)> = dataset_dirs
        .iter()
        .filter(|(_, dir, _)| read_data_mode(dir).ok() == Some(DataMode::Manual))
        .map(|(slug, _, c)| (slug.clone(), c.name.clone()))
        .collect();
    let mut rows_by_slug = fetch_dataset_rows(&client, ctx.paths, &manual, progress).await?;
```

and replace the loop's row block with:

```rust
        if read_data_mode(dataset_dir)? == DataMode::Manual {
            let (count, rows) = match rows_by_slug.remove(slug) {
                Some(v) => v,
                // Only reachable if the flag became readable between the
                // offline scan above and here; fall back to a direct fetch so
                // behaviour is identical either way.
                None => {
                    let mut one =
                        fetch_dataset_rows(&client, ctx.paths, &[(slug.clone(), c.name.clone())], progress)
                            .await?;
                    one.remove(slug).expect("single-slug fetch returns its slug")
                }
            };
            let (data_changed, data_conflicts) =
                apply_dataset_rows(ctx, slug, count, rows, progress).await?;
            conflicts += data_conflicts;
            if data_changed {
                changed.insert(slug.clone());
            }
        }
```

Note this moves `ctx.paths` reads next to `&mut ctx` uses — `fetch_dataset_rows` borrows `ctx.paths` immutably and completes before `apply_dataset_rows` takes `&mut ctx`, so the borrows do not overlap. If the borrow checker disagrees at the `process` site, bind `let paths = ctx.paths;` once at the top of `process` and pass `paths`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --locked --lib pull::mdh::`
Expected: PASS — including the three existing `pull_dataset_data_*` tests, which go through the unchanged wrapper and must not need edits. `pull_dataset_data_refuses_a_collection_over_the_hard_limit` is the one that proves the guardrail survived the split.

- [ ] **Step 5: Commit**

```bash
git add src/cli/pull/mdh.rs
git commit -m "perf(mdh): fetch manual datasets' rows concurrently

N manual datasets cost N x (count + find) in series. Split into a
concurrent fetch and a sequential apply, with the $count guardrail
still gating find WITHIN a dataset -- refusing an oversized collection
before reading it is the whole point of that ordering.

Projected, not measured: the test org has no manual datasets."
```

---
### Task 9: The push concurrency primitive, and `rules` as the pattern-setter

Spec **D9**, **D10**. Every update-capable push driver has the same shape (C9): per slug, do network work that needs only `&Lockfile`, then apply the result to the working tree and the lockfile, which needs `&mut Lockfile`. The network half is the slow half and the items are independent; the apply half must stay sequential and in slug order. B10 confirms today's loop never overlaps two PATCHes.

`rules` is the simplest driver and becomes the pattern the other three tasks follow. Rule PATCHes measured 6.56 / 6.23 req/s (B4) against a 10 req/s bucket, so the headroom here is modest **by design** — the point of doing `rules` first is the pattern, not the seconds.

**Order preservation.** The spec's R1 accepts reordered progress lines. This plan does better on the push path: `prepare_all` uses `buffered`, not `buffer_unordered`, so results come back in **input order**, and every progress line, disk write and lockfile write happens on the sequential stage. Push output stays byte-for-byte in today's slug order. R1 remains accepted only for the read paths (Tasks 3–8), where `list_remote` already documents it.

**Files:**
- Create: `src/cli/push/concurrent.rs`
- Modify: `src/cli/push/mod.rs` (`mod concurrent;`)
- Modify: `src/cli/push/rules.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks (the core bucket already exists).
- Produces:
  - `pub const PUSH_FANOUT: usize = 5;`
  - `pub enum Prepared<T> { Patched { slug: String, updated: T }, NeedsPrompt { slug: String }, Skipped { slug: String, event: String } }` with `pub fn slug(&self) -> &str`.
  - `pub async fn prepare_all<I, T, F, Fut>(items: I, prepare: F) -> Vec<Result<Prepared<T>>>` where `F: Fn(I::Item) -> Fut, Fut: Future<Output = Result<Prepared<T>>>` — one result per item, **in input order**, every item polled to completion even after one fails.
  - Tasks 10, 11 and 12 all consume exactly these three items. `T` is each driver's own response type, so a driver that must carry extra state to its apply stage (hooks carry deferred refs and a secrets hash) defines a private struct and uses it as `T`.

- [ ] **Step 1: Write the failing tests for the primitive**

Create `src/cli/push/concurrent.rs` containing only the test module for now (the implementation lands in Step 3):

```rust
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
            peak.load(Ordering::SeqCst) <= PUSH_FANOUT,
            "at most PUSH_FANOUT may be in flight, saw {}",
            peak.load(Ordering::SeqCst),
        );
    }
}
```

Add `mod concurrent;` to `src/cli/push/mod.rs` alongside the other `mod` lines.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rdc --locked --lib push::concurrent`
Expected: FAIL to compile — `Prepared`, `prepare_all` and `PUSH_FANOUT` do not exist.

- [ ] **Step 3: Write the primitive**

Prepend to `src/cli/push/concurrent.rs`, above the test module:

```rust
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
```

- [ ] **Step 4: Run tests to verify the primitive passes**

Run: `cargo test -p rdc --locked --lib push::concurrent`
Expected: PASS (3 tests).

- [ ] **Step 5: Write the failing tests for the `rules` driver**

Append inside the existing `#[cfg(test)] mod tests` in `src/cli/push/rules.rs`. These three pin the whole D9/D10 contract; Tasks 10–12 mirror them per kind.

```rust
    /// Shared fixture: `n` rules already in the lockfile with a matching base,
    /// so every one of them is a clean UPDATE. Returns (paths, lockfile,
    /// changes, the remote list body).
    fn seed_rules(
        tmp: &tempfile::TempDir,
        api: &str,
        slugs: &[&str],
    ) -> (Paths, Lockfile, BTreeMap<String, std::path::PathBuf>, serde_json::Value) {
        let paths = Paths::for_env(tmp.path(), "dev");
        let rules_dir = paths.rules_dir();
        std::fs::create_dir_all(&rules_dir).unwrap();
        let mut lockfile = Lockfile { api_base: api.to_string(), ..Lockfile::default() };
        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 700 + i as u64;
            let local = serde_json::json!({
                "name": slug,
                "url": format!("rdc://rules/{slug}"),
                "queues": [],
                "trigger": "annotation_content",
                "rule_actions": []
            });
            std::fs::write(
                rules_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&local).unwrap(),
            )
            .unwrap();
            lockfile.upsert("rules", slug, ObjectEntry {
                id, modified_at: None, modified_by: None,
                content_hash: None, secrets_hash: None,
            });
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/rules/{id}"),
                "name": slug,
                "queues": [],
                "trigger": "annotation_content",
                "rule_actions": []
            });
            let remote_rule: crate::model::Rule =
                serde_json::from_value(remote.clone()).unwrap();
            let (rj, rc) = serialize_rule(&remote_rule).unwrap();
            let base = rule_combined_hash(&rj, &rc, &lockfile);
            lockfile.upsert("rules", slug, ObjectEntry {
                id, modified_at: None, modified_by: None,
                content_hash: Some(base), secrets_hash: None,
            });
            changes.insert(slug.to_string(), rules_dir.join(format!("{slug}.json")));
            remotes.push(remote);
        }
        let list = serde_json::json!({
            "pagination": { "next": null }, "results": remotes
        });
        (paths, lockfile, changes, list)
    }

    /// Spec D9: clean updates PATCH concurrently. Four rules whose PATCHes each
    /// take 200ms cost ~800ms in series and ~200-400ms fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_rules_patches_updates_concurrently() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, list) =
            seed_rules(&tmp, &api, &["r-a", "r-b", "r-c", "r-d"]);

        Mock::given(method("GET"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for (i, slug) in ["r-a", "r-b", "r-c", "r-d"].iter().enumerate() {
            let id = 700 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/rules/{id}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(list["results"][i].clone())
                        .set_delay(std::time::Duration::from_millis(200)),
                )
                .mount(&server)
                .await;
            let _ = slug;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let start = std::time::Instant::now();
        let (pushed, skipped) =
            push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
                .await
                .expect("push should succeed");
        let elapsed = start.elapsed();

        assert_eq!((pushed, skipped), (4, 0));
        assert!(
            elapsed < std::time::Duration::from_millis(650),
            "four 200ms PATCHes must overlap; sequential would be >= 800ms, took {elapsed:?}",
        );
    }

    /// Spec D9: a drifted item is never PATCHed on the concurrent path. It is
    /// deferred to the sequential pass, where non-interactive `resolve_push_drift`
    /// skips it — so its id must never appear in a PATCH, while its clean
    /// neighbours are patched normally.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_rules_never_patches_a_drifted_item_concurrently() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, mut list) =
            seed_rules(&tmp, &api, &["r-a", "r-b", "r-c"]);
        // r-b (id 701) drifted: the remote now carries a name the recorded
        // base never saw, so its combined hash no longer matches.
        list["results"][1]["name"] = serde_json::json!("changed remotely");

        Mock::given(method("GET"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in [0usize, 2] {
            let id = 700 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/rules/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(list["results"][i].clone()))
                .mount(&server)
                .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let (pushed, skipped) =
            push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
                .await
                .expect("push should succeed");

        assert_eq!((pushed, skipped), (2, 1), "the drifted rule is skipped");
        let patched: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == http::Method::PATCH)
            .map(|r| r.url.path().to_string())
            .collect();
        assert!(
            !patched.iter().any(|p| p.ends_with("/rules/701")),
            "the drifted rule must never be PATCHed, saw {patched:?}"
        );
        assert_eq!(patched.len(), 2, "only the two clean rules are patched");
    }

    /// Spec D10: when one item's PATCH fails, every PATCH that DID complete is
    /// still recorded before the error propagates. The sequential loop aborted
    /// with the failing item's siblings unrecorded; this must be strictly
    /// better, not worse.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_rules_records_completed_patches_when_one_fails() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let (paths, mut lockfile, changes, list) =
            seed_rules(&tmp, &api, &["r-a", "r-b", "r-c"]);
        let before: Vec<Option<String>> = ["r-a", "r-b", "r-c"]
            .iter()
            .map(|s| lockfile.objects["rules"][*s].content_hash.clone())
            .collect();

        Mock::given(method("GET"))
            .and(path("/api/v1/rules"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in [0usize, 2] {
            let id = 700 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/rules/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(list["results"][i].clone()))
                .mount(&server)
                .await;
        }
        // 400 is NOT retriable, so this fails immediately instead of burning
        // the retry budget.
        Mock::given(method("PATCH"))
            .and(path("/api/v1/rules/701"))
            .respond_with(ResponseTemplate::new(400).set_body_string("nope"))
            .mount(&server)
            .await;

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let err = push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
            .await
            .expect_err("the failing PATCH must propagate");
        assert!(format!("{err:#}").contains("701"), "error names the failed rule: {err:#}");

        let after: Vec<Option<String>> = ["r-a", "r-b", "r-c"]
            .iter()
            .map(|s| lockfile.objects["rules"][*s].content_hash.clone())
            .collect();
        assert_ne!(after[0], before[0], "r-a's completed PATCH must be recorded");
        assert_ne!(after[2], before[2], "r-c's completed PATCH must be recorded");
        assert_eq!(after[1], before[1], "the failed rule's base must not move");
    }
```

Add `use wiremock::matchers::{method, path};` and `use wiremock::{Mock, MockServer, ResponseTemplate};` to the test module if the existing `use` lines do not already cover them (they do), and add `http` to the test module's imports for `http::Method`.

- [ ] **Step 6: Run tests to verify they fail**

Run: `cargo test -p rdc --locked --lib push::rules`
Expected: `push_rules_patches_updates_concurrently` FAILS on the elapsed assertion (~800ms sequential). The other two may pass by accident today — that is fine and expected; they are regression pins for the refactor, not drivers of it. Confirm the timing test fails before proceeding.

- [ ] **Step 7: Refactor the driver**

Rewrite `src/cli/push/rules.rs`'s `push` as: partition → sequential creates → hoisted drift list → concurrent prepare → sequential apply. Extract the two blocks the apply stage reuses.

```rust
pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    use crate::cli::push::concurrent::{Prepared, prepare_all};

    let rules_dir = paths.rules_dir();
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    // CREATEs stay strictly sequential: POST assigns ids that later items
    // resolve against, so that ordering is load-bearing.
    let mut creates: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    let mut updates: Vec<(&String, &std::path::PathBuf)> = Vec::new();
    for (slug, path) in changes {
        if lockfile.objects.get("rules").and_then(|m| m.get(slug.as_str())).is_none() {
            creates.push((slug, path));
        } else {
            updates.push((slug, path));
        }
    }

    for (slug, local_json_path) in creates {
        // ... the existing CREATE block, verbatim, minus only its trailing
        // `continue;` (it is now the whole loop body). Its own
        // `progress.event(Action::Post, ...)` and `pushed += 1;` stay where
        // they are — do NOT add a second increment here.
    }

    if updates.is_empty() {
        return Ok((pushed, skipped));
    }

    // Drift-check list, hoisted to ONE fetch before the batch. Same single
    // request the lazy `remote_rules` cache used to make — it just no longer
    // sits behind the first item's PATCH.
    let remote_rules = client
        .list_rules(Some(progress.clone()))
        .await
        .context("listing rules to verify no drift before push")?;

    // === Concurrent stage. Needs only `&Lockfile`; touches neither the
    //     working tree nor the lockfile, and never prompts.
    let lf: &Lockfile = &*lockfile;
    let remote_ref = &remote_rules;
    let dir_ref = &rules_dir;
    let prepared = prepare_all(updates.iter().copied(), |(slug, _path)| async move {
        let entry = lf
            .objects
            .get("rules")
            .and_then(|m| m.get(slug.as_str()))
            .expect("partitioned as an update, so the entry exists");
        let Some(base) = entry.content_hash.clone() else {
            return Ok(Prepared::Skipped {
                slug: slug.clone(),
                event: format!("rule/{slug} (no content_hash)"),
            });
        };
        let id = entry.id;

        let mut payload = read_rule_value(dir_ref, slug)
            .with_context(|| format!("reading local rule '{slug}'"))?;
        crate::snapshot::refs::resolve_value(&mut payload, lf);
        let payload_rule: crate::model::Rule = serde_json::from_value(payload)
            .with_context(|| format!("deserializing overlay-applied rule '{slug}'"))?;

        let Some(remote_rule) = remote_ref.iter().find(|r| r.id == id) else {
            return Ok(Prepared::Skipped {
                slug: slug.clone(),
                event: format!("rule/{slug} (remote id {id} missing)"),
            });
        };
        let (remote_json, remote_code) = serialize_rule(remote_rule)?;
        if rule_combined_hash(&remote_json, &remote_code, lf) != base {
            // Drift. NOT patched here — the sequential stage owns the prompt.
            return Ok(Prepared::NeedsPrompt { slug: slug.clone() });
        }

        let mut payload_to_send = payload_rule;
        strip_patch_extra(&mut payload_to_send.extra, "rules", false);
        let updated = client
            .update_rule(id, &payload_to_send, Some(progress.clone()))
            .await
            .with_context(|| format!("PATCH /rules/{id}"))?;
        Ok(Prepared::Patched { slug: slug.clone(), updated })
    })
    .await;

    // === Sequential apply stage, in the driver's existing slug order. Owns
    //     `&mut Lockfile`, the filesystem and every prompt. Every completed
    //     PATCH is recorded even if a sibling failed (spec D10), then the
    //     first error propagates.
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (_slug, local_json_path)) in prepared.into_iter().zip(updates) {
        match item {
            Ok(Prepared::Patched { slug, updated }) => {
                write_back(paths, &rules_dir, lockfile, &slug, local_json_path, &updated)?;
                progress.event(Action::Patch, &format!("rule/{slug}"));
                pushed += 1;
            }
            Ok(Prepared::Skipped { event, .. }) => {
                progress.event(Action::Skip, &event);
                skipped += 1;
            }
            Ok(Prepared::NeedsPrompt { slug }) => {
                let (p, s) = push_one_drifted(
                    paths, client, lockfile, interactive, &rules_dir, &slug,
                    local_json_path, &remote_rules, progress, env,
                )
                .await?;
                pushed += p;
                skipped += s;
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }
    if let Some(e) = first_error {
        return Err(e);
    }

    Ok((pushed, skipped))
}

/// Write one PATCH response back: canonical form to disk and the base cache,
/// the `.py` sidecar (or its removal), and the lockfile entry.
///
/// Lifted verbatim out of the old update loop — the block from
/// `let (updated_json, updated_code) = serialize_rule(&updated)?;` down to and
/// including the `lockfile.upsert("rules", ...)` call, with `updated` taken by
/// reference and the `progress.event(Action::Patch, ...)` line left behind at
/// the call site so the caller controls when it fires. The moved block's
/// `local_py_path` is recomputed here as `rules_dir.join(format!("{slug}.py"))`,
/// exactly as the loop head used to compute it.
fn write_back(
    paths: &Paths,
    rules_dir: &std::path::Path,
    lockfile: &mut Lockfile,
    slug: &str,
    local_json_path: &std::path::Path,
    updated: &crate::model::Rule,
) -> Result<()> {
    // ... moved block ...
    Ok(())
}

/// Resolve one drifted rule interactively and, on `Patch`, send it.
///
/// This is the old update loop's drift branch, moved verbatim: re-read the
/// local file, `resolve_value`, `resolve_push_drift`, then either PATCH (via
/// the same `update_rule` + `write_back`), adopt the remote, or skip. It runs
/// only on the sequential stage, so `resolve_push_drift`'s prompt can never
/// interleave with another item's. Returns `(pushed, skipped)` deltas.
#[allow(clippy::too_many_arguments)]
async fn push_one_drifted(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    rules_dir: &std::path::Path,
    slug: &str,
    local_json_path: &std::path::Path,
    remote_rules: &[crate::model::Rule],
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    // ... moved block ...
}
```

Two details the compiler will make you get right, both harmless:

1. `let lf: &Lockfile = &*lockfile;` must be dropped before the apply stage takes `&mut lockfile`. It is: `prepared` owns no borrow of `lf` (every `Prepared` field is owned), so the reborrow ends at the `.await`. If NLL disagrees, wrap the concurrent stage in a block that returns `prepared`.
2. `prepare_all`'s closure is `Fn`, not `FnMut`, so it must not capture anything by mutable reference. Everything it touches (`lf`, `remote_ref`, `dir_ref`, `client`, `progress`) is shared.

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p rdc --locked --lib push::rules`
Expected: PASS — the three new tests plus the pre-existing `push_patch_rule_caches_code_sidecar_to_base`, which pins the write-back this task moved into `write_back`.

Run: `cargo test -p rdc --locked --test cli_sync rule`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src/cli/push/concurrent.rs src/cli/push/mod.rs src/cli/push/rules.rs
git commit -m "perf(push): concurrent network stage, sequential apply stage

Adds the push fan-out primitive and lands it on rules as the pattern.
buffered (not buffer_unordered) so results arrive in input order and
the whole transcript keeps its slug order; drifted items are deferred
to the sequential pass so a prompt can never interleave; every
completed PATCH is recorded before the first error propagates."
```

---

### Task 10: `hooks` — the biggest measured push win

Spec **D9**, **D10**. Hook PATCHes measured **2.44 req/s** (B5) with a 396ms mean and a 1064ms max (L4) against a 10 req/s bucket — **4.1× of headroom**, the largest on the write path. This is the task the push work exists for.

Hooks carry two things `rules` does not, and both must travel from the concurrent stage to the apply stage rather than being recomputed:

- **deferred refs** from `resolve_value_deferring` (C7), which the apply stage accumulates into `relink`;
- the **secrets hash** returned by `inject_hook_secrets`, which lands in the lockfile entry.

So `T` is a driver-local struct rather than the bare response.

**Out of scope, staying sequential:** the create path (including `create_hook_from_template` installs), and the secrets-only force-push pass at the bottom of `push`. The secrets pass is a different shape (it is driven by the secrets file, not by `changes`) and was never measured; leave it alone.

**Files:**
- Modify: `src/cli/push/hooks.rs`

**Interfaces:**
- Consumes: Task 9's `PUSH_FANOUT`, `Prepared`, `prepare_all`.
- Produces: no signature change — `hooks::push(paths, client, lockfile, interactive, changes, catalog_hooks, relink, progress, env) -> Result<(usize, usize)>` is untouched. Driver-private:

```rust
/// What a hook's concurrent stage carries across to its apply stage.
struct HookPatched {
    updated: crate::model::Hook,
    /// Fields held back from the PATCH by `resolve_value_deferring`; the apply
    /// stage turns these into `relink::DeferredRelink` entries.
    deferred: Vec<(String, serde_json::Value)>,
    /// From `inject_hook_secrets`, for the lockfile entry.
    secrets_hash: Option<String>,
}
```

- [ ] **Step 1: Write the failing test**

Append inside the existing `#[cfg(test)] mod tests` in `src/cli/push/hooks.rs`:

```rust
    /// Spec D9/B5: hook PATCHes ran at 2.44 req/s against a 10 req/s bucket —
    /// the largest headroom on the write path. Four hooks whose PATCHes each
    /// take 300ms cost ~1.2s in series and ~300-600ms fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_hooks_patches_updates_concurrently() {
        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let hooks_dir = paths.hooks_dir();
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let slugs = ["h-a", "h-b", "h-c", "h-d"];
        let mut lockfile = Lockfile { api_base: api.clone(), ..Lockfile::default() };
        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 900 + i as u64;
            let local = serde_json::json!({
                "name": slug,
                "url": format!("rdc://hooks/{slug}"),
                "type": "webhook",
                "queues": [],
                "events": [],
                "config": { "url": "https://example.invalid/hook" }
            });
            std::fs::write(
                hooks_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&local).unwrap(),
            )
            .unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/hooks/{id}"),
                "name": slug,
                "type": "webhook",
                "queues": [],
                "events": [],
                "config": { "url": "https://example.invalid/hook" }
            });
            lockfile.upsert("hooks", slug, ObjectEntry {
                id, modified_at: None, modified_by: None,
                content_hash: None, secrets_hash: None,
            });
            let remote_hook: crate::model::Hook =
                serde_json::from_value(remote.clone()).unwrap();
            let (rj, rc) = serialize_hook(&remote_hook).unwrap();
            let base = hook_combined_hash(&rj, &rc, &lockfile);
            lockfile.upsert("hooks", slug, ObjectEntry {
                id, modified_at: None, modified_by: None,
                content_hash: Some(base), secrets_hash: None,
            });
            changes.insert(slug.to_string(), hooks_dir.join(format!("{slug}.json")));
            remotes.push(remote);
        }
        let list = serde_json::json!({ "pagination": { "next": null }, "results": remotes });

        Mock::given(method("GET"))
            .and(path("/api/v1/hooks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in 0..slugs.len() {
            let id = 900 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/hooks/{id}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(list["results"][i].clone())
                        .set_delay(std::time::Duration::from_millis(300)),
                )
                .mount(&server)
                .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let mut relink = Vec::new();
        let start = std::time::Instant::now();
        let (pushed, skipped) = push(
            &paths, &client, &mut lockfile, false, &changes, &[], &mut relink, &progress, "dev",
        )
        .await
        .expect("push should succeed");
        let elapsed = start.elapsed();

        assert_eq!((pushed, skipped), (4, 0));
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "four 300ms PATCHes must overlap; sequential would be >= 1.2s, took {elapsed:?}",
        );
    }
```

The argument order above is `hooks::push`'s real one, as `push_classified` calls it: `(paths, client, lockfile, interactive, changes, catalog_hooks, relink, progress, env)`. `catalog_hooks` is `&[]` here because this test exercises no store extensions.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --locked --lib push::hooks::tests::push_hooks_patches_updates_concurrently`
Expected: FAIL on the elapsed assertion (~1.2s sequential).

- [ ] **Step 3: Refactor the driver**

Apply Task 9's exact shape to the update path only:

1. Partition `changes` into creates and updates as in Task 9, keying on the absence of a `lockfile.objects["hooks"]` entry. Run the create loop unchanged, sequentially.
2. Hoist the `drift_hooks` lazy list to one `client.list_hooks(...)` before the batch.
3. The concurrent stage runs, per slug: `read_hook_value` → `hook_code_extension_from_value` → `resolve_value_deferring` → typed deserialize → find the remote by id (missing → `Prepared::Skipped` with the existing `hook/{slug} (remote id {id} missing)` wording) → `serialize_hook` + `hook_combined_hash` → drift → `Prepared::NeedsPrompt` → otherwise build the `Value` body, remove every deferred key from it, `strip_for_create(&mut body, "hooks")`, `inject_hook_secrets`, `update_hook_value`, and return:

```rust
        Ok(Prepared::Patched {
            slug: slug.clone(),
            updated: HookPatched { updated, deferred, secrets_hash },
        })
```

4. The sequential apply stage mirrors Task 9's `match`, with the `Patched` arm calling a `write_back` extracted from the block that starts at `let (updated_json, updated_code) = serialize_hook(&updated)?;` — passing `deferred` and `secrets_hash` through so the `relink` accumulation and the `secrets_hash` field of the lockfile entry are unchanged.
5. `push_one_drifted` for hooks is the existing drift branch moved verbatim, including its `deferred = resolve_value_deferring(&mut ov, lockfile)` recomputation on an edited payload and its `Adopt` arm writing both `.json` and the code sidecar.
6. Leave everything after the update loop — the secrets-only force-push pass and its summary — exactly as it is.

The apply-stage skeleton is identical to Task 9's:

```rust
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (_slug, local_json_path)) in prepared.into_iter().zip(updates) {
        match item {
            Ok(Prepared::Patched { slug, updated }) => { /* write_back + relink + event + pushed += 1 */ }
            Ok(Prepared::Skipped { event, .. }) => { progress.event(Action::Skip, &event); skipped += 1; }
            Ok(Prepared::NeedsPrompt { slug }) => { /* push_one_drifted */ }
            Err(e) => { if first_error.is_none() { first_error = Some(e); } }
        }
    }
    if let Some(e) = first_error { return Err(e); }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --locked --lib push::hooks`
Expected: PASS — the new test plus all five pre-existing `push_*_hook_*` tests. Two of them are the real gate on this refactor: `push_patch_hook_defers_unresolvable_run_after_instead_of_failing` and `push_patch_hook_omits_a_deferred_queues_field_rather_than_emptying_it` pin the deferred-ref handling that now crosses the stage boundary (spec R2).

Run: `cargo test -p rdc --locked --test cli_sync hook`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/push/hooks.rs
git commit -m "perf(push): fan out hook PATCHes

Hook PATCHes ran at 2.44 req/s against a 10 req/s bucket -- 4.1x of
headroom, the largest on the write path. Deferred refs and the secrets
hash travel from the concurrent stage to the sequential apply stage in
a driver-local struct rather than being recomputed."
```

---
### Task 11: The remaining cached-list drivers

Spec **D9**, **D10**. `labels`, `engines`, `engine_fields`, `queues` and `email_templates` all have `rules`' exact shape: a lazily-fetched full-kind list used for the drift check, then a PATCH per slug. Apply Task 9's pattern to each.

These were not individually measured — B4/B5 covered rules and hooks. They are included because leaving half the write path sequential would make the push transcript's timing depend on which kinds happened to change, and because the pattern is now fixed and cheap to apply. Expect modest wins on kinds with few changed objects and a real one on a large `queues` or `email_templates` push.

**Per-driver notes, each verified against the current tree:**

| driver | drift list | extra state to carry | keep sequential |
|---|---|---|---|
| `labels` | `client.list_labels` (~line 112) | none | creates |
| `engines` | `client.list_engines` (~line 133) | deferred refs → `relink` (like hooks) | creates |
| `engine_fields` | `client.list_engine_fields` (~line 118) | none | creates |
| `queues` | `client.list_queues` (~line 126) | deferred refs → `relink` | creates |
| `email_templates` | `client.list_email_templates` (~line 282) | none | **the whole create branch** — its `pick_adoption_id` / `claimed` logic threads a mutable `claimed` set and a `remote_cache` across iterations and upserts the lockfile mid-loop. It is stateful by construction; do not touch it. |

**Files:**
- Modify: `src/cli/push/labels.rs`, `src/cli/push/engines.rs`, `src/cli/push/engine_fields.rs`, `src/cli/push/queues.rs`, `src/cli/push/email_templates.rs`

**Interfaces:**
- Consumes: Task 9's `PUSH_FANOUT`, `Prepared`, `prepare_all`; Task 10's `HookPatched` precedent for the two drivers that carry deferred refs (`engines`, `queues`) — each defines its own equivalent, e.g. `struct QueuePatched { updated: crate::model::Queue, deferred: Vec<(String, serde_json::Value)> }`.
- Produces: no signature changes. Every driver's `push(...)` keeps its current parameter list and `Result<(usize, usize)>` return.

- [ ] **Step 1: Write the failing test for one representative driver**

`src/cli/push/labels.rs` has **no** test module today, so create one at the end of the file (it is the simplest of the five — no deferred refs, no sidecars):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::codec::combined_hash;

    /// Spec D9: clean label updates PATCH concurrently. Four labels whose
    /// PATCHes each take 200ms cost ~800ms in series and ~200-400ms fanned out.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_labels_patches_updates_concurrently() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let labels_dir = paths.labels_dir();
        std::fs::create_dir_all(&labels_dir).unwrap();

        let slugs = ["l-a", "l-b", "l-c", "l-d"];
        let mut lockfile = Lockfile { api_base: api.clone(), ..Lockfile::default() };
        let mut changes = BTreeMap::new();
        let mut remotes = Vec::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 500 + i as u64;
            let local = serde_json::json!({
                "name": slug,
                "url": format!("rdc://labels/{slug}"),
                "color": "#112233"
            });
            std::fs::write(
                labels_dir.join(format!("{slug}.json")),
                serde_json::to_vec_pretty(&local).unwrap(),
            )
            .unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/labels/{id}"),
                "name": slug,
                "color": "#112233"
            });
            lockfile.upsert("labels", slug, ObjectEntry {
                id, modified_at: None, modified_by: None,
                content_hash: None, secrets_hash: None,
            });
            let codec = crate::snapshot::codec::codec("labels").unwrap();
            let art = codec.disk_bytes(&remote).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert("labels", slug, ObjectEntry {
                id, modified_at: None, modified_by: None,
                content_hash: Some(base), secrets_hash: None,
            });
            changes.insert(slug.to_string(), labels_dir.join(format!("{slug}.json")));
            remotes.push(remote);
        }
        let list = serde_json::json!({ "pagination": { "next": null }, "results": remotes });

        Mock::given(method("GET"))
            .and(path("/api/v1/labels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list.clone()))
            .mount(&server)
            .await;
        for i in 0..slugs.len() {
            let id = 500 + i as u64;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/labels/{id}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(list["results"][i].clone())
                        .set_delay(std::time::Duration::from_millis(200)),
                )
                .mount(&server)
                .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let start = std::time::Instant::now();
        let (pushed, skipped) =
            push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
                .await
                .expect("push should succeed");
        let elapsed = start.elapsed();

        assert_eq!((pushed, skipped), (4, 0));
        assert!(
            elapsed < std::time::Duration::from_millis(650),
            "four 200ms PATCHes must overlap; sequential would be >= 800ms, took {elapsed:?}",
        );
    }
}
```

`use super::*;` brings in `Paths`, `Lockfile`, `ObjectEntry` and `BTreeMap` from the driver's own `use` block; `combined_hash` is already imported there too, so the extra `use` line above is belt-and-braces — drop it if the compiler calls it redundant.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --locked --lib push::labels::tests::push_labels_patches_updates_concurrently`
Expected: FAIL on the elapsed assertion (~800ms sequential).

- [ ] **Step 3: Refactor `labels`**

Apply Task 9's shape verbatim: partition creates/updates on the absence of a `lockfile.objects["labels"]` entry, run creates sequentially, hoist `client.list_labels(...)` to one call, then:

```rust
    let lf: &Lockfile = &*lockfile;
    let remote_ref = &remote_labels;
    let prepared = prepare_all(updates.iter().copied(), |(slug, path)| async move {
        let entry = lf.objects.get("labels").and_then(|m| m.get(slug.as_str()))
            .expect("partitioned as an update, so the entry exists");
        let Some(base) = entry.content_hash.clone() else {
            return Ok(Prepared::Skipped {
                slug: slug.clone(),
                event: format!("label/{slug} (no content_hash)"),
            });
        };
        let id = entry.id;
        // ... read `path`, resolve refs, typed deserialize (existing code) ...
        let Some(remote) = remote_ref.iter().find(|l| l.id == id) else {
            return Ok(Prepared::Skipped {
                slug: slug.clone(),
                event: format!("label/{slug} (remote id {id} missing)"),
            });
        };
        // ... codec disk_bytes + combined_hash (existing code) ...
        if remote_combined != base {
            return Ok(Prepared::NeedsPrompt { slug: slug.clone() });
        }
        // ... strip_patch_extra + client.update_label (existing code) ...
        Ok(Prepared::Patched { slug: slug.clone(), updated })
    })
    .await;
```

followed by the identical apply-stage skeleton:

```rust
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (_slug, path)) in prepared.into_iter().zip(updates) {
        match item {
            Ok(Prepared::Patched { slug, updated }) => { /* write_back + event + pushed += 1 */ }
            Ok(Prepared::Skipped { event, .. }) => { progress.event(Action::Skip, &event); skipped += 1; }
            Ok(Prepared::NeedsPrompt { slug }) => { /* push_one_drifted */ }
            Err(e) => { if first_error.is_none() { first_error = Some(e); } }
        }
    }
    if let Some(e) = first_error { return Err(e); }
```

with the driver's existing write-back block extracted to `write_back` and its existing drift branch extracted to `push_one_drifted`, both moved verbatim — exactly as Task 9 did for `rules`.

- [ ] **Step 4: Run the labels tests**

Run: `cargo test -p rdc --locked --lib push::labels`
Expected: PASS.

- [ ] **Step 5: Repeat for `engine_fields`, `engines`, `queues`, `email_templates`**

Same partition / hoist / prepare / apply shape, one driver at a time, running that driver's filtered tests after each. Two carry extra state:

- `engines`: `resolve_value_deferring` produces deferred refs the apply stage feeds into `relink`. Carry them in a driver-local `struct EnginePatched { updated: crate::model::Engine, deferred: Vec<(String, serde_json::Value)> }`, exactly as Task 10 does with `HookPatched`.
- `queues`: identical, with `struct QueuePatched { updated: crate::model::Queue, deferred: Vec<(String, serde_json::Value)> }`.

`email_templates`: leave the entire create branch (the `pick_adoption_id` / `claimed` / `remote_cache` logic, roughly lines 95–280) untouched and sequential; split only the update branch below it. Its drift list at ~line 282 is the one to hoist.

Run after each driver: `cargo test -p rdc --locked --lib push::<driver>`
Expected: PASS.

- [ ] **Step 6: Confirm intra-kind ordering is still satisfied (spec R2)**

For each of the five drivers, confirm one of these two holds and note which in the commit body:

- the kind resolves same-kind refs through `resolve_value_deferring` + `push::relink` (so ordering within the kind is not load-bearing) — true for `engines` and `queues`, both of which pass a `relink` vector; or
- the kind has **no same-kind refs at all**, so only the cross-kind dispatch order in `push_classified` matters, and that is unchanged — check by grepping the kind's codec and model for a self-referencing field.

Run: `cargo test -p rdc --locked --test cli_sync`
Expected: PASS. This is the suite that exercises real dispatch order end to end.

- [ ] **Step 7: Commit**

```bash
git add src/cli/push/labels.rs src/cli/push/engines.rs src/cli/push/engine_fields.rs src/cli/push/queues.rs src/cli/push/email_templates.rs
git commit -m "perf(push): fan out the remaining cached-list drivers

labels, engines, engine_fields, queues and email_templates take the
same concurrent-network / sequential-apply split. Creates stay
sequential everywhere, and email_templates' adoption branch -- which
threads a claimed set and a remote cache across iterations -- is left
alone entirely."
```

---

### Task 12: The per-item-GET drivers

Spec **D9**, **D10**. `schemas`, `workspaces` and `inboxes` do not list; they fetch each object by id for the drift check. That per-item GET is exactly the kind of round trip the split exists to overlap — and for `inboxes` there is a **second** one, the post-PATCH re-baseline `get_inbox`, which must move into the concurrent stage with its PATCH rather than being left on the apply path.

**Request counts must not change.** `schemas` caches its GETs by schema id (`remote_cache`), which dedupes when two slugs resolve to the same schema. Fanning the GET out naively inside each item's future would re-fetch a shared schema once per slug. Prefetch the **distinct** ids concurrently before the batch instead, then have the concurrent stage read the map. Same count, now overlapped.

`workspaces` and `inboxes` have one id per slug by construction, so their GETs go inside the per-item future with no dedup concern.

**Files:**
- Modify: `src/cli/push/schemas.rs`, `src/cli/push/workspaces.rs`, `src/cli/push/inboxes.rs`

**Interfaces:**
- Consumes: Task 9's `PUSH_FANOUT`, `Prepared`, `prepare_all`.
- Produces: no signature changes. `inboxes` defines `struct InboxPatched { refetched: crate::model::Inbox }` — the apply stage writes back the **re-fetched** body, not the PATCH response, because the PATCH response omits fields the GET includes and recording the PATCH-derived shape causes a spurious `RemoteEdit` on the next sync (the existing comment in `inboxes.rs` explains this; it must survive the refactor).

- [ ] **Step 1: Write the failing test**

`src/cli/push/workspaces.rs` has **no** test module today, so create one at the end of the file (it is the simplest per-item-GET driver):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::codec::combined_hash;

    /// Spec D9: the per-item drift GET plus its PATCH is two round trips per
    /// slug. Four workspaces at 150ms per call cost ~1.2s in series; fanned
    /// out they cost roughly one slug's worth.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_workspaces_overlaps_the_per_item_drift_get_and_patch() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api = format!("{}/api/v1", server.uri());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");

        let slugs = ["w-a", "w-b", "w-c", "w-d"];
        let mut lockfile = Lockfile { api_base: api.clone(), ..Lockfile::default() };
        let mut changes = BTreeMap::new();
        for (i, slug) in slugs.iter().enumerate() {
            let id = 300 + i as u64;
            let ws_dir = paths.workspace_dir(slug);
            std::fs::create_dir_all(&ws_dir).unwrap();
            let ws_path = ws_dir.join("workspace.json");
            let local = serde_json::json!({
                "name": slug,
                "url": format!("rdc://workspaces/{slug}"),
                "queues": []
            });
            std::fs::write(&ws_path, serde_json::to_vec_pretty(&local).unwrap()).unwrap();
            let remote = serde_json::json!({
                "id": id,
                "url": format!("{api}/workspaces/{id}"),
                "name": slug,
                "queues": []
            });
            lockfile.upsert("workspaces", slug, ObjectEntry {
                id, modified_at: None, modified_by: None,
                content_hash: None, secrets_hash: None,
            });
            let codec = crate::snapshot::codec::codec("workspaces").unwrap();
            let art = codec.disk_bytes(&remote).unwrap();
            let base = combined_hash(&art.json, &art.sidecars, &lockfile);
            lockfile.upsert("workspaces", slug, ObjectEntry {
                id, modified_at: None, modified_by: None,
                content_hash: Some(base), secrets_hash: None,
            });
            changes.insert(slug.to_string(), ws_path);

            let slow = |body: serde_json::Value| {
                ResponseTemplate::new(200)
                    .set_body_json(body)
                    .set_delay(std::time::Duration::from_millis(150))
            };
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/workspaces/{id}")))
                .respond_with(slow(remote.clone()))
                .mount(&server)
                .await;
            Mock::given(method("PATCH"))
                .and(path(format!("/api/v1/workspaces/{id}")))
                .respond_with(slow(remote.clone()))
                .mount(&server)
                .await;
        }

        let client = crate::api::RossumClient::new(api.clone(), "TEST".into()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let start = std::time::Instant::now();
        let (pushed, skipped) =
            push(&paths, &client, &mut lockfile, false, &changes, &progress, "dev")
                .await
                .expect("push should succeed");
        let elapsed = start.elapsed();

        assert_eq!((pushed, skipped), (4, 0));
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "the four GET+PATCH pairs must overlap; sequential would be >= 1.2s, took {elapsed:?}",
        );
    }
}
```

`Paths::workspace_dir(slug)` is `<root>/envs/<env>/workspaces/<slug>/` and the file inside it is `workspace.json` — which is exactly what the driver's `changes` map points at.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --locked --lib push::workspaces::tests::push_workspaces_overlaps_the_per_item_drift_get_and_patch`
Expected: FAIL on the elapsed assertion (~1.2s sequential).

- [ ] **Step 3: Refactor `workspaces`**

Task 9's shape, with the per-item `client.get_workspace(id, …)` moving **inside** the concurrent future — it is the drift check, and per C9 the drift check needs only `&Lockfile`:

```rust
    let lf: &Lockfile = &*lockfile;
    let prepared = prepare_all(updates.iter().copied(), |(slug, ws_path)| async move {
        let entry = lf.objects.get("workspaces").and_then(|m| m.get(slug.as_str()))
            .expect("partitioned as an update, so the entry exists");
        let Some(base) = entry.content_hash.clone() else {
            return Ok(Prepared::Skipped {
                slug: slug.clone(),
                event: format!("workspace/{slug} (no content_hash)"),
            });
        };
        let id = entry.id;
        // ... existing read + resolve_value + typed deserialize ...
        let remote_workspace = client
            .get_workspace(id, Some(progress.clone()))
            .await
            .with_context(|| format!("fetching workspace {id} to verify drift before push"))?;
        // ... existing codec disk_bytes + combined_hash ...
        if remote_combined != base {
            return Ok(Prepared::NeedsPrompt { slug: slug.clone() });
        }
        // ... existing strip + client.update_workspace ...
        Ok(Prepared::Patched { slug: slug.clone(), updated })
    })
    .await;
```

followed by the same apply-stage skeleton as Tasks 9–11:

```rust
    let mut first_error: Option<anyhow::Error> = None;
    for (item, (_slug, ws_path)) in prepared.into_iter().zip(updates) {
        match item {
            Ok(Prepared::Patched { slug, updated }) => { /* write_back + event + pushed += 1 */ }
            Ok(Prepared::Skipped { event, .. }) => { progress.event(Action::Skip, &event); skipped += 1; }
            Ok(Prepared::NeedsPrompt { slug }) => { /* push_one_drifted */ }
            Err(e) => { if first_error.is_none() { first_error = Some(e); } }
        }
    }
    if let Some(e) = first_error { return Err(e); }
```

- [ ] **Step 4: Run the workspaces tests**

Run: `cargo test -p rdc --locked --lib push::workspaces`
Expected: PASS.

- [ ] **Step 5: Refactor `inboxes`, carrying the re-fetch into the concurrent stage**

Same shape, plus: after `client.update_inbox(...)` the driver re-fetches with `client.get_inbox(id, …)` to re-baseline. Move that GET into the concurrent future, immediately after the PATCH, and return the **re-fetched** body:

```rust
struct InboxPatched {
    /// The GET-derived body, not the PATCH response. The PATCH response omits
    /// fields the GET includes (e.g. `bounce_email_to: null`), and recording
    /// the PATCH-derived shape makes the next sync's classifier see a spurious
    /// RemoteEdit and re-pull the queue bundle. See the existing comment in
    /// this file.
    refetched: crate::model::Inbox,
}
```

so the apply stage's write-back reads `refetched` exactly where it reads the re-fetch result today. Do not leave the re-fetch on the apply path — that would put a round trip back into the sequential stage and undo half the win.

Run: `cargo test -p rdc --locked --lib push::inboxes`
Expected: PASS.

- [ ] **Step 6: Refactor `schemas`, prefetching distinct ids**

`schemas` caches drift GETs by schema id, so two slugs sharing an id pay one GET. Preserve that exactly: before the batch, collect the distinct ids from the update partition's lockfile entries and fetch them concurrently into the map the concurrent stage then reads.

```rust
    // Drift bodies, prefetched by DISTINCT schema id so a schema shared by two
    // slugs still costs exactly one GET — the same dedup the sequential
    // `remote_cache` gave us, now overlapped instead of serialized.
    let mut ids: Vec<u64> = updates
        .iter()
        .filter_map(|(slug, _)| {
            lockfile.objects.get("schemas").and_then(|m| m.get(slug.as_str())).map(|e| e.id)
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let remote_cache: std::collections::BTreeMap<u64, crate::model::Schema> = {
        use futures::stream::{StreamExt, TryStreamExt};
        let fetched: Vec<(u64, crate::model::Schema)> = futures::stream::iter(ids)
            .map(|id| async move {
                let s = client
                    .get_schema(id, Some(progress.clone()))
                    .await
                    .with_context(|| format!("fetching schema {id} to verify drift before push"))?;
                Ok::<_, anyhow::Error>((id, s))
            })
            .buffered(crate::cli::push::concurrent::PUSH_FANOUT)
            .try_collect()
            .await?;
        fetched.into_iter().collect()
    };
```

Then the concurrent stage reads `remote_cache[&id]` and never fetches, and the rest is Task 9's shape.

Run: `cargo test -p rdc --locked --lib push::schemas`
Expected: PASS.

- [ ] **Step 7: Run the full sync integration suite**

Run: `cargo test -p rdc --locked --test cli_sync`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/cli/push/schemas.rs src/cli/push/workspaces.rs src/cli/push/inboxes.rs
git commit -m "perf(push): fan out the per-item-GET drivers

schemas, workspaces and inboxes fetch each object by id for the drift
check; that GET now overlaps with its siblings. inboxes' post-PATCH
re-baseline GET moves into the concurrent stage with its PATCH.
schemas prefetches DISTINCT ids so a shared schema still costs one GET."
```

---

### Task 13: Verify, re-measure, and publish before/after

Spec's **Expected results** table is projected, not measured, and says so. This task replaces it with numbers, regressions included. Nothing in this plan is finished until this task has run.

**Files:**
- Modify: `docs/superpowers/specs/2026-08-24-sync-parallelization-design.md` (the Expected results table becomes measured)
- Modify: `README.md` and/or `docs/` only if the trace flag warrants a user-facing mention — decide after Step 6, do not pre-commit to it

**Interfaces:**
- Consumes: every prior task.
- Produces: no code. A measured table and an honest statement of what is still projected.

- [ ] **Step 1: Full suite**

Run: `cargo test -p rdc --locked`
Expected: PASS, all binaries. Note the lib and `cli_sync` counts in the commit body.

- [ ] **Step 2: Lint**

Run: `cargo clippy -p rdc --all-targets --locked -- -D warnings`
Expected: **exactly these three pre-existing errors and no others** — they were already on `main` at this plan's base commit `05678ee`, in files no task here touches, under a local toolchain (rustc/clippy 1.95.0) that is ahead of what the repo was last linted against:

- `src/cli/sync/mod.rs:886` — `collapsible_if` (from `f077d62`)
- `src/cli/migrate/mod.rs:3556` — `field_reassign_with_default` (from `9192bb9`)
- `src/cli/migrate/mod.rs:3699` — `field_reassign_with_default` (from `9192bb9`)

A fourth finding, or any finding in a file this plan touched, is a regression — fix it. Do not fix these three: they are outside this plan's scope and this tree is shared with a concurrent worker, so editing `sync/mod.rs` or `migrate/mod.rs` risks colliding with work in progress. Do **not** run `cargo fmt` either — this repo is not fmt-clean under the local rustfmt and a repo-wide format would bury the change.

- [ ] **Step 3: Property tests explicitly**

Run: `cargo test -p rdc --locked classify`
Expected: PASS. The classifier is safety layer 1 and this plan never touched it; this is the check that says so.

- [ ] **Step 4: Build the release binary the measurement will use**

Run: `cargo build -p rdc --release --locked`

The previously-measured binary was built before this work and before the 20 organization-settings commits that landed mid-design, so the before/after must **not** reuse it. Record `git rev-parse --short HEAD` and use one commit for both halves: measure "before" by checking out the pre-Task-1 parent into a separate worktree and building there, or by `git stash`-free comparison against a tag you create now. Do not compare a new binary against an old measurement.

- [ ] **Step 5: Re-run the live harness**

Run the opt-in 8-scenario live integration harness against the test org exactly as it is normally invoked (see `tests/live.rs` and `tests/live/` for the opt-in environment variable and the token it expects — the token comes from the user's environment, never from this repo).
Expected: 8/8 green, matching the last recorded run.

- [ ] **Step 6: Measure**

For each row of the spec's Expected results table, run the command with `RDC_TRACE_HTTP` pointed at a scratch file, three times, and take the median wall clock:

```bash
TRACE_DIR=$(mktemp -d)
for i in 1 2 3; do
  RDC_TRACE_HTTP="$TRACE_DIR/no-push-$i.csv" \
    /usr/bin/time -p ./target/release/rdc sync <env> --no-push
done
```

and likewise for `sync <env> --dry-run`. For the two push rows, reproduce the same edit shapes B4 and B5 used (40 rule edits; 14 hook edits) and measure the PATCH phase from the trace: the span from the first to the last `PATCH` line, and the achieved req/s over that span.

From each CSV, compute and record:

- total requests, split core vs Data Storage (Data Storage lines have `POST` descs containing `/svc/data-storage/`);
- achieved req/s per phase;
- total `limiter_wait_ms` — B6 measured **zero** blocked time before this work; if it is now non-zero and large on the Data Storage side, D1's 30/s constant is the thing to revisit, not the fan-out.

- [ ] **Step 7: Verify the invariant this whole plan rests on**

Compare the per-command **request counts** against the pre-change binary for the same project and env:

- steady `sync --no-push`: 58 requests (15 core, 43 Data Storage) in the baseline run.
- steady `sync --dry-run`: same 58.

Expected: identical. A difference is a bug in Task 7's prefetch scoping or Task 12's schema dedup, not a rounding artefact — chase it before publishing anything.

- [ ] **Step 8: Rewrite the spec's Expected results table**

Replace the projected table with measured medians, keeping the "why" column, and change the sentence above it from "Projected from S/L/B, **not yet measured**" to a statement of when and against what it was measured. Report every regression. Then update the two claims that this plan knowingly leaves standing:

- **D8 stays projected** (R6): the test org has no manual datasets. Say so in the table's note, not in a footnote.
- **The dry-run row forecast stays sequential** (Task 8's out-of-scope note): record it as a known remaining gap, with the reason — giving it the guardrail would add a request per dataset, and giving it a guardrail-free variant would fork the helper.
- **D6's scope was tightened** from "local dataset dir exists" to "local `indexes.json` exists" (Task 7). Amend D6 to match the code.

- [ ] **Step 9: Commit**

```bash
git add docs/superpowers/specs/2026-08-24-sync-parallelization-design.md
git commit -m "docs: replace the projected sync-parallelization results with measurements

Medians of three runs per command on one binary and one commit, with
request counts verified unchanged per command. D8 stays projected --
the measurement org has no manual datasets -- and the dry-run row
forecast is recorded as a known remaining gap."
```

- [ ] **Step 10: Report honestly**

Summarise for the user: measured before/after per command, the request-count invariant result, what regressed if anything, and the two things still unverified (D8, the dry-run row forecast). Do not round a 1.4× up to "roughly 2×", and do not present the projected rows as measured.

---

## Self-Review

**Spec coverage.** Every decision maps to a task: D1 → 2, D2 → 2 (asserted, not changed), D3 → 3, D4 → 3 + 5, D5 → 6, D6 → 7, D7 → 4, D8 → 8, D9 → 9/10/11/12, D10 → 9 (and inherited by 10/11/12), D11 → 1. Non-goals N1–N4 appear in Global Constraints as prohibitions. Risks: R1 is narrowed (the push path now preserves order via `buffered`; R1 remains accepted only for the read paths), R2 is a required check in Task 11 Step 6 and is pinned by two existing hook tests in Task 10 Step 4, R3 is a Global Constraint, R4 is documented on the constructor in Task 2, R5 is the `NeedsPrompt` split in Task 9, R6 is stated in Task 8 and re-stated in Task 13 Step 8. The spec's Testing section maps to: unit → Tasks 2/3/4/9; integration request-count assertions → Task 7 Step 5 and Task 13 Step 7; drift-never-patched-concurrently → Task 9 Step 5; D10 → Task 9 Step 5; property tests → Task 13 Step 3; live → Task 13 Step 5.

**Two deliberate deviations from the spec**, both recorded above and both requiring a spec amendment in Task 13 Step 8:

1. **D6's prefetch scope is tightened** from "local dataset dir exists" to "local `indexes.json` exists". The spec claimed the dry-run request count would be "unchanged or lower"; with the dir predicate it could be *higher* for a dataset dir that has no `indexes.json`. The tighter predicate makes it exactly unchanged.
2. **The push path preserves order** (`buffered`, plus every progress line emitted on the sequential stage), so R1 does not apply to Tasks 9–12. This is strictly stronger than the spec asked for.

**Type consistency.** `Prepared<T>` / `prepare_all` / `PUSH_FANOUT` are defined once in Task 9 and consumed unchanged in 10, 11 and 12. `MdhListed::new` and `MdhListed::datasets` are defined in Task 4 and consumed in 5, 6, 7 and 8. `MdhListed::index_sets` is added in Task 7 and read in 7 only. `fetch_index_sets` is defined in Task 3 with the signature Tasks 5 and 7 call. `fetch_dataset_rows` / `apply_dataset_rows` are defined and consumed within Task 8. The per-driver carry structs (`HookPatched`, `EnginePatched`, `QueuePatched`, `InboxPatched`) are each private to their driver and named at their point of definition.
