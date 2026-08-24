# Faster `rdc sync` — parallelism inside the measured ceilings

**Status:** design, awaiting review
**Date:** 2026-08-24

## Problem

`rdc sync` is slower than it needs to be, but not for the reason the code
suggests. The obvious lever — "raise the fan-out" — is wrong on the core API and
right only on the Data Storage side, and the single worst offender is a command
that performs no writes at all: `sync --dry-run` takes **2.1× the wall clock of
the real sync it previews**, because its MDH forecast loop is fully sequential.

Three separate things are true at once, and only measurement separates them:

- **The core API is already at its ceiling.** Its list phase runs at 8.5–11.2
  req/s against a 10 req/s per-token bucket. Nothing on that path can go faster
  without issuing fewer requests.
- **Data Storage was never paced and is where the concurrency win lives.** It
  carries 72% of a sync's requests (43 of 58), throttles independently of the
  core API, and has no client-side limiter at all.
- **The push path is sequential and kind-dependent.** Rule PATCHes already reach
  6.6 req/s and have little headroom; hook PATCHes reach 2.44 req/s and have
  4.1×.

**Goal.** Saturate each service's own budget, overlap the two services instead
of running them back to back, and stop the write path from waiting one round
trip at a time — without touching a single one of sync's four safety layers.

**Non-goal.** Issuing fewer requests. That is the only remaining lever on the
core path and it is a separate, safety-bearing design (see N1, N2).

## Verified facts

Nothing below is inferred. Service facts come from probes run 2026-08-24 against
a test org on `api.elis.rossum.ai/v1` and its Data Storage service; rdc-level
numbers come from the real release binary traced through
`retry::send_with_retry`; code facts cite the tree.

### Service limits

| # | Fact | Consequence |
|---|---|---|
| S1 | Core API bursts of 15 / 30 / 60 concurrent GETs each yielded exactly **11 × 200** then 429 with `Retry-After: 1`. | Burst 10 + one refill. The documented 10 req/s policy holds. |
| S2 | 60 requests paced at exactly 10/s with concurrency 10 → **1 × 429**. | `RateLimiter::rossum_core_api()` (10/s, burst 10) is correctly calibrated. Leave it alone. |
| S3 | Data Storage: 80 concurrent POSTs → **0 × 429** (~143 req/s). | No burst ceiling reachable at rdc's scale. |
| S4 | Data Storage: 320 requests at concurrency 20 → 36–63 req/s, **0 × 429**, p95 flat at 0.59–0.72s. | No windowed limiter engages under sustained load either. |
| S5 | On one token, the core API 429s at its 11th concurrent request **while** Data Storage serves 80 concurrently. | The two services throttle **independently**. One shared bucket would spend core tokens on calls nobody asked us to pace. |
| S6 | Data Storage exposes 24 paths (its own `openapi.json`). `indexes/list` and `search_indexes/list` both **require** a single `collectionName` (422 without it). `collections/list` with `nameOnly:false` returns only the implicit `_id_` index. | Index discovery costs **exactly 2 calls per dataset**. There is no bulk form to optimise into. |
| S7 | Neither service returns any rate-limit header. The `x-limiter-core-api` header quoted in `api/rate_limit.rs` is **no longer present**. | That doc comment's mechanism is stale even though its numbers are right. Re-verify before relying on the header. |

### Latencies

| # | Fact |
|---|---|
| L1 | Core list: queues 275ms, schemas 372ms, hooks 805ms (184KB body), small kinds 54–100ms. |
| L2 | Core GET by id (schema body): 191–820ms. |
| L3 | Core PATCH of a rule, through rdc: **133–140ms** mean over 80 PATCHes across two runs. |
| L4 | Core PATCH of a hook, through rdc: **396ms** mean, 1064ms max, over 14 PATCHes. |
| L5 | DS `indexes/list` 111–246ms; `search_indexes/list` **214–372ms**; `collections/list` 233–546ms. |
| L6 | Concurrency does not inflate write latency: 8 concurrent no-op PATCHes → all 200, wall 0.26s vs sequential median 0.236s. No 409s, no ordering errors. |

### Baselines and achieved rates

Medians of repeated runs against a snapshot of 3 queues, 34 hooks, 96 rules,
15 email templates, 21 MDH datasets.

| # | Fact |
|---|---|
| B1 | steady `sync --no-push`: **3.1s** (2.37–3.86, n=6). 58 requests — 15 core, 43 DS. |
| B2 | steady `sync --dry-run`: **6.5s** (6.16–6.96, n=4). Same 58 requests. **2.1× the sync it previews.** |
| B3 | first full pull: 10.1s. |
| B4 | push of 40 rule edits: PATCH phase 6.10s / 6.42s = **6.56 / 6.23 req/s**. |
| B5 | push of 14 hook edits: PATCH phase 5.74s = **2.44 req/s**. |
| B6 | **Zero** time blocked on the rate limiter in every traced run. |
| B7 | Core listing achieves **8.5–11.2 req/s** — at its 10/s ceiling. |
| B8 | MDH index fetch during sync achieves **18.4 / 38.5 / 25.4 req/s** — unpaced. |
| B9 | MDH index fetch during dry-run: 42 requests in **6.75s at max concurrency 3** = 6.2 req/s, i.e. 79% of that run's 8.52s wall clock. |
| B10 | Push PATCHes never overlap (max concurrent 2–3, an artefact of header-vs-body timing; the loop is sequential by construction). |

### Code facts

| # | Fact |
|---|---|
| C1 | **Both** clients funnel every request through `retry::send_with_retry` — a single pacing and instrumentation point. DS passes `limiter: None` (`api/data_storage.rs:409`). |
| C2 | `fetch_index_set` (`cli/pull/mdh.rs:544`) awaits `list_indexes` **then** `list_search_indexes`. Two independent calls, serialized. |
| C3 | `plan_mdh_index_edits` (`cli/pull/mdh.rs:370`) is a plain sequential `for` loop. |
| C4 | MDH `collections/list` is one arm of the **same** `buffer_unordered(PULL_FANOUT)` as the 13 core list kinds (`cli/pull/common.rs:187`–`302`). Traced start: t=1.05s, queued behind 11 core lists. |
| C5 | `slug_to_collection` (`cli/sync/execute.rs:3764`) covers **every** catalog collection, and the `subset` handed to `process` (`execute.rs:3965`) is all of them. So a real sync already fetches index sets for every collection. |
| C6 | **Four** sites duplicate the same slug computation over `catalog.mdh.collections`: `mdh.rs:186`, `mdh.rs:384`, `mdh.rs:752`, `execute.rs:3764` — all `crate::slug::slugify_unique` in listing order. `plan_mdh_index_edits` carries a comment warning that a divergence would silently target the wrong collection. |
| C7 | `resolve_value_deferring(&mut Value, &Lockfile)` borrows the lockfile **immutably** and runs on **both** the create and update paths for hooks / queues / engines. |
| C8 | **No test asserts HTTP request order or push event order.** Every `received_requests()` assertion uses `.find()` or `.filter().count()`; the one stderr assertion (`tests/cli_sync.rs:10210`) is a single `contains`. |
| C9 | Push write-back needs `&mut Lockfile` (`lockfile.upsert`) and the filesystem; everything before the PATCH needs only `&Lockfile`. |
| C10 | `DataStorageClient` is `#[derive(Clone)]`, so an `Arc<RateLimiter>` field gives every clone one shared bucket — matching the per-token server scope. |

## Design

### Pacing

**D1 — Data Storage gets its own limiter, same mechanism.** Add
`RateLimiter::rossum_data_storage()` = **30/s, burst 30**, stored as
`Arc<RateLimiter>` on `DataStorageClient` and threaded into `send_envelope`
(replacing the `None` at `data_storage.rs:409`). The doc comment records S3/S4
the way `rate_limit.rs` already records the core policy, and S7 — that the
header is gone and the numbers come from probes.

*Accepted cost:* this paces the fastest observed runs **down** from 38.5 to 30
req/s (B8). That is deliberate. B8 is one cluster at one moment; 30/s sits far
below anything that throttled and still leaves the win intact.

**D2 — the core limiter is unchanged.** S2 says it is right.

### MDH read path

**D3 — `fetch_index_set` uses `tokio::try_join!`.** Per dataset, cost drops from
`indexes + search_indexes` to `max(...)` — in one traced run, 125ms + 285ms
becomes 285ms.

**D4 — one shared `fetch_index_sets` helper.** Signature
`fetch_index_sets(&DataStorageClient, &[(slug, name)], &Arc<Log>) -> Result<BTreeMap<String, IndexSet>>`:
`try_join!` within a dataset (D3), `buffer_unordered(MDH_FANOUT)` across
datasets. Used by **both** `process` sub-phase B and `plan_mdh_index_edits`,
which fixes C3 — the single largest measured defect (B9).

`MDH_FANOUT = 10`. At L5 latencies that attempts 27–90 req/s, which D1's bucket
then paces to 30. **The bucket is the governor, not the fan-out**, so a larger
constant buys nothing and only widens the failure blast radius.

Each call site keeps its **own scope**. `plan_mdh_index_edits` still skips
datasets with no local `indexes.json`; only the scheduling changes. Request
counts per command are unchanged.

**D5 — MDH listing leaves the core stream.** `list_remote` runs the MDH arm as a
sibling future (`tokio::join!`) alongside the core `buffer_unordered` stream,
instead of as its 13th arm (C4). With separate buckets (D1, S5) the two phases
genuinely overlap rather than queueing behind one another.

**D6 — index sets are prefetched during listing, scoped to collections whose
local dataset dir exists.** That predicate is decidable offline from `paths`, and
it sits exactly between the two consumers' needs:

- dry-run needs collections with a local `indexes.json` — a **subset** of it;
- sync needs **all** collections (C5) — a superset.

So `process` fetches only the remainder, which is the new-collection set and is
normally empty. Consequences, each checked against a case:

| case | effect |
|---|---|
| steady-state sync | whole MDH phase overlaps core listing |
| first pull (no local dirs) | nothing prefetched; identical cost to today |
| dry-run | request count unchanged or lower, now concurrent |
| new remote collection | its 2 calls happen in `process`, as today |

**D7 — the slug map is computed once during listing and carried on the
catalog.** The four duplicated computations (C6) collapse to one. This is a
prerequisite for D6 — the prefetch must slug-match `process` exactly — and it
retires the mis-targeting hazard `plan_mdh_index_edits` already warns about.

**D8 — MDH row pulls fan out across datasets.** `pull_dataset_data` runs
sequentially inside sub-phase C's apply loop. Split it the same way as D4: fetch
concurrently across manual datasets, apply sequentially. The `count_documents`
guardrail must still gate `find_all` **within** a dataset — that ordering is
load-bearing and stays.

*Unmeasured:* the test org has no manual datasets, so this one is designed by
analogy to D4 rather than from a measurement. It is included because the row
ceiling was just raised to 25k (`snapshot/mdh_data.rs:31`), so multi-dataset row
pulls are clearly a real shape. It must be reported as projected, never as
verified, until a manual dataset exists to measure.

### Push path

**D9 — each kind splits into a concurrent network stage and a sequential apply
stage.** Per C9 the boundary is already clean:

- **Concurrent**, bounded at `PUSH_FANOUT`, needs only `&Lockfile`: read local
  file → `resolve_value_deferring` → drift-check against the cached list,
  hoisted to **one** fetch before the batch (today it is lazily fetched inside
  the loop) or the per-item GET that `schemas` / `workspaces` / `inboxes` still
  do → build body → `strip_for_create` → PATCH → return the response.
- **Sequential**, in the existing slug order, needs `&mut Lockfile` + FS:
  serialize → `portabilize_proposed` → combined hash → disk and base-cache
  writes → `lockfile.upsert` → relink accumulation.
- **Creates stay sequential.** POST assigns ids that later items resolve
  against; that ordering is load-bearing.
- **A drifted item is not PATCHed concurrently.** It returns a needs-prompt
  marker and is handled in a sequential pass, so `resolve_push_drift` prompts
  can never interleave.

`PUSH_FANOUT = 5`. At L3/L4 that attempts 12–37 req/s, paced by the core bucket
to 10. Again the bucket governs.

**D10 — a mid-batch failure still records every PATCH that completed.** The
apply stage runs for all successes, then the first error propagates. Today an
abort leaves at most one PATCH unrecorded; naive concurrency would widen that to
`PUSH_FANOUT`. This makes the inconsistency window **smaller than today's**, not
larger.

### Instrumentation

**D11 — `RDC_TRACE_HTTP=<path>` becomes a real opt-in feature** in
`send_with_retry` (C1): one CSV line per attempt —
`epoch_ms, limiter_wait_ms, duration_ms, status, desc` — behind a `OnceLock`, so
a disabled trace costs one atomic load. Every number in this document was
produced by it. It is how D1–D10 get verified and how a future regression gets
diagnosed instead of guessed at.

## Expected results

Projected from S/L/B, **not yet measured**. To be re-measured with D11 and
reported honestly, regressions included.

| command | today | projected | why |
|---|---|---|---|
| steady `sync --no-push` | 3.1s | **~2.0s** | core (1.5s) and MDH (0.5s list + 42 reqs @ 30/s) overlap instead of summing |
| steady `sync --dry-run` | 6.5s | **~2.2s** | B9's serial loop becomes 42 requests at 30/s |
| push 14 hooks | 5.74s | **~1.4s** | 2.44 → 10 req/s |
| push 40 rules | 6.10s | **~4.0s** | 6.56 → 10 req/s; little headroom by design |
| first full pull | 10.1s | ~9s | mostly core-bound; D6 prefetches nothing on a fresh tree |

## Non-goals

**N1 — raising `PULL_FANOUT` / `LIST_PAGE_FANOUT`, or overlapping
`prefetch_queue_schemas` with listing.** B7 shows core listing already runs at
its 10/s ceiling, so overlapping only moves requests around under the same cap.
For 20 queues: `(13+20)/10 = 3.3s` overlapped versus `1.4 + 2.0 = 3.4s` today —
noise, for real complexity. This was in an earlier draft and the measurement
killed it.

**N2 — reducing the number of MDH requests.** S6 makes 2 calls per dataset a
floor. A change-gate that skipped the fetch would trade away detection of a
remote-only index edit — a safety change that deserves its own design, not a
line in a performance one.

**N3 — parallelizing deletes.** `push::deletes::run_deletes` is cascade-ordered
(children before parents) and that ordering is load-bearing.

**N4 — a global cross-service scheduler.** Two buckets and per-call-site
fan-out are enough to reach both ceilings. A scheduler would risk the
deterministic ordering and prompt semantics the current code carefully
preserves, for no measured gain.

## Risks

| # | Risk | Mitigation |
|---|---|---|
| R1 | Progress lines commit in completion order, so `patch` lines reorder. | C8: nothing asserts it, and `list_remote` already documents accepting this. Summary counters are tallied from the classification, not the loop, so they are unaffected. |
| R2 | Intra-kind push ordering. | C7 for hooks/queues/engines: same-kind refs defer and relink, on both paths. The kinds that do **not** defer have no same-kind refs; their cross-kind refs are satisfied by the driver dispatch order, which D9 preserves. To be re-confirmed per driver during implementation, not assumed. |
| R3 | Sync's four safety layers. | Untouched. Layer 3's per-item GET + hash compare stays per item — only its scheduling changes. Layer 1 is offline, Layers 2 and 4 are not on this path. |
| R4 | A cluster whose DS limit is below S3/S4 → 429 storm. | D1's bucket is the first line; `send_with_retry`'s existing `Retry-After` handling is the second. The 30/s constant is one line to change. |
| R5 | Interactive prompts under concurrency. | D9's needs-prompt split keeps every prompt on the sequential path; `stdin_coord` remains the single stdin owner. |
| R6 | D8 is projected, not measured. | Report it as such until a manual dataset exists to measure against. |

## Testing

- **Unit.** `rossum_data_storage()` bucket shape (mirroring the existing
  `rate_limit` tests, `start_paused` clock); `fetch_index_sets` returns a map
  keyed identically to the sequential path; the D9 split produces a
  byte-identical lockfile and on-disk tree to the sequential path (golden
  compare).
- **Integration (wiremock).** Assert **request counts per command are
  unchanged** — the parallelization must neither add nor drop a request. Assert a
  drifted item still prompts and is never PATCHed concurrently. Assert D10: a
  mid-batch failure records every completed PATCH.
- **Property.** The `classify` property tests are untouched and must stay green.
- **Live.** Re-run the 8-scenario live harness, then re-measure the Expected
  results table with D11 and publish before/after.
