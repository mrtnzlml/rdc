# A stateful fake org: making convergence testable offline

## Problem

`rdc` is unstable in three ways that cost real time today: a sync reports
changes when nothing changed and never settles; one rejected object aborts a
run and leaves a project half-advanced; and a promotion chain has to be run
repeatedly with a human watching the prompts.

They share one cause. **`rdc` encodes what the Rossum API will accept and what
it returns as a few dozen hand-learned facts, scattered across codecs, driver
branches, pre-flight classes and reconcilers — and the only mechanism that can
validate any of them is a live run against a real org.** From that single
deficiency:

- **Churn** — `rdc` guesses wrong about what a write *returns*, records a base
  that no pull would produce, and classifies the object drifted forever after.
- **Mid-run failure** — `rdc` guesses wrong about what the API will *accept*,
  the server 400s, and the run aborts at the first `?`. The only repair is
  another sync, which is why churn and mid-run failure compound each other.
- **Promotion never settling** — `migrate` guesses wrong about what a legal
  target object looks like, so each cycle rewrites what the last one wrote.

The measuring instrument for all three is currently the maintainer. It is
serialized, slow, and covers only the paths he happens to walk. The comment at
`src/cli/push/organization.rs:161` says it exactly:

> The obvious thing — write the PATCH response — is wrong, and it took a live
> organization to show it. […] Every mock-based test missed it, because a mock
> naturally answers GET and PATCH with the same body.

This design builds the missing instrument: a **stateful fake organization** the
existing scenario suite can run against, so "nothing should happen the second
time" becomes an assertion in `cargo test`.

## Verified facts

All measured in this repo on 2026-09-07 unless dated otherwise.

### The offline suite cannot express convergence

- `cargo test --all-targets`: **1660 passed, 0 failed, 23 ignored**, ~25 s.
- The 23 ignored are the live scenarios (`#[ignore = "live: needs RDC_LIVE_*
  env"]` plus a runtime `LiveConfig::from_env()` early return).
- Offline tests fake the API with **stateless `wiremock` canned responses**: a
  `PATCH` never changes what a later `GET` returns. Test comments say so
  outright — "Single label, served identically on every listing call"
  (`tests/cli_sync.rs:617`).
- Statefulness is already *wanted* and faked by call counting: 14 tests drive
  an `Arc<AtomicUsize>` inside a `respond_with` closure to sequence "phase 1 /
  phase 2" responses (e.g. `tests/cli_sync.rs:844`).
- `assert_converged` — the assertion this design needs — exists only in the
  live harness (`tests/live/support/converge.rs:280`).

### The write-back law is repeated, not encapsulated

The post-write ritual is: `codec.disk_bytes` → `portabilize_proposed` →
`combined_hash` → `base_cache::write_disk_and_cache` → `lockfile.upsert`
(canonical example: `src/cli/push/labels.rs:367`'s `write_back`). Its surface area:

| site | count |
| --- | --- |
| `codec.disk_bytes(` call sites | 149 |
| `portabilize_proposed` call sites (28 files) | 77 |
| `base_cache::write_disk_and_cache` call sites | 34 |
| hand-written per-driver `write_back` fns | 11 |
| push driver LoC across 19 files | 18,528 |

And the *decision* the ritual depends on — is a PATCH response a faithful
picture of the object's post-write state? — is made per driver, three
incompatible ways:

| strategy | drivers |
| --- | --- |
| write the PATCH response | labels, engines, engine_fields, queues, rules, saved_views, workspaces |
| re-fetch with a GET, write that | inboxes (`refetched`), schemas, partly hooks / email_templates |
| write only the managed subtree, merged into the pull-shaped local file | organization |

### Push is fail-fast at three nested levels

- per object: `let created = result?;` (`src/cli/push/labels.rs`, the create arm)
- per driver: every `...push(...).await?` in `push_classified`
  (`src/cli/push/mod.rs:88+`)
- and dispatch is dependency-ordered, so an early rejection blocks everything
  behind it.

There is no run-level journal, rollback or resume; individual file writes are
atomic (`snapshot::writer::write_atomic`), the run as a whole is not.

### The fix rate is flat

`fix:` commits per month, May→Sep 2026: **61 / 55 / 41 / 74 / 24**. By scope:
`fix(sync)` 41, `fix(desktop)` 25, `fix(migrate)` 24, `fix(push)` 16,
`fix(deploy)` 16, `fix(mdh)` 12, `fix(pull)` 9 — roughly 60% of all scoped
fixes across 1199 commits sit in the sync pipeline.

### The API surface a fake must model is small and closed

- **28 core endpoints** in `src/api/mod.rs`, **59** client methods, plus **14**
  Data Storage methods in `src/api/data_storage.rs`.
- Every request funnels through one function, `retry::send_with_retry`
  (`src/api/retry.rs:106`; 7 callers in `api/mod.rs`, 1 in `api/data_storage.rs`).
- Auth is a header — `Authorization: token <t>` (`src/api/mod.rs:400`).
  `POST /v1/auth/login` matters only to `rdc auth` and the 401 refresh path.

### The live harness is already backend-parameterized

| already true | evidence |
| --- | --- |
| no scenario hardcodes a host, org id or token | `tests/live/support/config.rs:31` |
| the fixture builds a project from `EnvCreds` | `ProjectFixture::init` writes `rdc.toml` + `secrets/<env>.secrets.json` |
| two-org promotion is parameterized | `ProjectFixture::init_envs` gives each env its own org and token |
| goldens are id-free and run-agnostic | `CapturedState` = lockfile slugs with the run-id stripped + `rdc://` refs (`tests/live/support/expected.rs`) |
| the convergence assertion is backend-agnostic | `assert_converged` drives the CLI, not the API |
| the binary is spawned as a subprocess | `assert_cmd::Command::cargo_bin("rdc")` in `ProjectFixture::run_rdc` — so the fake must be **real TCP**, not an in-process stub |
| `testdata/live/snapshot/**` is a hand-written INPUT fixture (unseeded, `rdc sync` POSTs it whole) — not an expected-pulled-output tree | `tests/live/support/snapshot.rs:1-12` |
| the seed graph is data-driven | `testdata/live/manifest.toml` + `testdata/live/bodies/**` |

### Server behaviors already documented in-tree

Each is a fact the fake must reproduce, with the citation that proves it:

| behavior | citation |
| --- | --- |
| list envelope `{pagination: {total, total_pages, next, previous}, results}`; `?page_size=100&ordering=id&page=N`; `total_pages == 0` → follow `next`; dedupe by `url` | `src/api/mod.rs:457-504` |
| `page_size` capped at 100 (default 20) | `src/api/mod.rs:60` |
| pages 2..N are fetched **in parallel** | `src/api/mod.rs:470` |
| `/schemas` list omits `content`; bodies must be fetched by id | `src/cli/pull/common.rs:66`, `:388`, `:419` |
| labels have no GET-by-id | `tests/live/scenarios/round_trip.rs:120` |
| `POST /queues` auto-creates typed default email templates; a blind POST of one 400s | `src/cli/push/email_templates.rs:97`, `:149`; the 5 unprefixed files under `testdata/live/snapshot/**/email-templates/` |
| `POST /queues` validates the schema's extracted field names against the bound engine's field names (`non_field_errors: Engine (id: N) restriction: …`) | `src/cli/push/mod.rs:88-96` |
| deleting a queue removes its auto-created schema and templates | `docs/superpowers/plans/2026-06-25-live-integration-test-harness.md:18` |
| a queue DELETE is async: `202 deletion_requested`, and the queue is not gone when it returns | `tests/live/support/teardown.rs:38-67` |
| deleting an engine still attached to a queue awaiting deletion → `400 engine_attached_to_queues_waiting_for_deletion` | `tests/live/support/teardown.rs:62` |
| an unresolvable ref → `400 Invalid hyperlink - No URL match` | `src/snapshot/refs.rs:159`, `src/snapshot/create.rs:66`, `:90` |
| field-length caps, with trailing whitespace trimmed *before* validation | `src/snapshot/limits.rs` |
| organization PATCH response is not GET-shaped: carries `rir_key`, reorders `users` | `src/cli/push/organization.rs:161-171` |
| organization normalizes `width: 140` → `140.0` and `annotation_list_table: {}` → `columns: []` | `src/cli/push/organization.rs:174-177` |
| inbox PATCH response omits fields GET includes (e.g. `bounce_email_to`) | `src/cli/push/inboxes.rs:377`, `:725` |

### MDH is absent by construction, not by stubbing

`api_base = <uri>/api/v1` derives the Data Storage base as
`<uri>/svc/data-storage/api` — the **same host and port**
(`src/config/mod.rs:29`). So the fake's own catch-all answers MDH requests, and
"if MDH isn't enabled on the target cluster, the resulting URL will return 404
— the pull driver tolerates that" (`src/config/mod.rs:33`), which
`pull_skips_mdh_when_endpoint_returns_404` and the comment at
`tests/cli_sync.rs:118` both exercise today. Stages 1-3 therefore need no MDH
stub at all, and stage 4 has its transport already wired.

### wiremock 0.6.5 supports what this needs

- `matchers::any()` — `src/matchers.rs:126`
- closures implement `Respond` with the bound `F: Send + Sync + Fn(&Request) ->
  ResponseTemplate` — `src/respond.rs:147`. **The handler is synchronous**, so
  state must be a `std::sync::Mutex` and all handler logic non-async.
- `MockServerBuilder::disable_request_recording()` — `src/mock_server/builder.rs:85`,
  so request history cannot grow unbounded.
- `MockServer::uri()` — `src/mock_server/exposed_server.rs:394`.

## Decisions

1. **Give the existing live scenario suite a second backend** rather than
   adding a stateful mock layer to `tests/cli_sync.rs`. The suite is already
   parameterized on `(api_base, org_id, token)`; the fake supplies those.
2. **`wiremock` catch-all as the transport.** Real TCP is mandatory because
   `run_rdc` spawns the real binary. If wiremock ever proves limiting, swapping
   in a hand-rolled hyper server is contained behind `FakeOrg::start()`.
3. **Strict validation.** The fake rejects what the server rejects. Mid-run
   400s are a named symptom; a permissive fake could not reproduce them.
4. **Model causality and ordering, not latency.** An async queue delete is
   "gone after the next poll", never a wall-clock wait.
5. **The fake invents nothing.** Every quirk cites the live scenario that
   proves it, enforced by a guard test.
6. **The 108 existing wiremock tests stay untouched.** They assert exact
   request shapes, which the fake does not replace.
7. **One test binary.** Fake variants live in `tests/live.rs` beside their live
   twins, so nothing compiles twice.
8. **MDH / Data Storage is out of scope** for stages 1–3.

## Design

### A. `FakeOrg` — the backend

```
FakeOrg::start()             -> FakeOrg          // binds a port, mounts one catch-all
FakeOrg::start_pair()        -> (FakeOrg, FakeOrg) // source + target org, for promotion
FakeOrg::creds(&self)        -> EnvCreds        // api_base = <uri>/api/v1, org_id, token
FakeOrg::config(&self)       -> LiveConfig      // source only
FakeOrg::config_pair(src, tgt) -> LiveConfig    // with `target: Some(tgt.creds())`
```

One `Mock::given(any()).respond_with(handler)` where `handler` owns
`Arc<Mutex<OrgState>>`. The handler routes on `(method, path)` and is entirely
synchronous. Built with `disable_request_recording()`.

Each test gets its own `FakeOrg`: own port, own state, no shared globals, so
the suite needs no `--test-threads=1` and cannot flake on cross-test
interference.

### B. `OrgState` — the object graph

- `objects: BTreeMap<&'static str /*kind*/, BTreeMap<u64 /*id*/, Value>>`
- monotonic id allocation per kind; `url` minted as `<api_base>/<kind>/<id>`, so
  `rdc://` portabilization is genuinely exercised
- `modified_at` bumped on every write from a deterministic logical clock (not
  the wall clock), so goldens stay stable
- `deleting: BTreeSet<u64>` for the async-delete state machine
- list responses in the Rossum envelope, honouring `page`, `page_size`
  (capped 100), `ordering=id`, `next`/`previous`, and reporting `total_pages`
- kind-specific list projections — `/schemas` strips `content`
- kind-specific endpoint absence — `GET /labels/{id}` → 404
- unmodelled kinds answer an empty list rather than 404

### C. `Quirks` — the learned-facts layer

One named rule per fact, each shaped as a hook into the request/response path:

```
/// `POST /queues` materializes the server's typed default email templates.
/// Proven by: tests/live/scenarios/email_templates.rs::live_email_templates_round_trip
fn on_queue_created(state: &mut OrgState, queue_id: u64) { … }

/// The organization PATCH response is not GET-shaped.
/// Proven by: tests/live/scenarios/organization.rs::live_organization_settings_push
fn shape_organization_patch_response(body: &mut Value) { … }
```

**Provenance guard.** A test walks the quirk registry and asserts every entry's
`Proven by:` names a test function that exists in `tests/live/scenarios/**`.
This is the same enforcement pattern the repo already uses in
`tests/command_references.rs`, `committed_template_regions_match_the_renderer`,
`kinds::PUSH_CAPABLE`, and `push_classified`'s deliberately exhaustive
destructuring. A quirk nobody can prove against a real org is a quirk someone
invented, and the build says so.

### D. Strict validation

Rejections modelled in stage 1, each mirroring a documented server behavior:

| request | response |
| --- | --- |
| a ref that resolves to no object | `400 Invalid hyperlink - No URL match` |
| a field longer than its cap, after trailing-whitespace trim | `400` naming the field |
| `POST /queues` with no schema | `400` |
| a queue carrying both `engine` and `generic_engine` | `400` |
| `POST /queues` whose schema extracts a field the bound engine lacks | `400 non_field_errors: Engine (id: N) restriction: …` |
| `POST /email_templates` duplicating an auto-created typed default | `400` |
| `DELETE /engines/{id}` while a queue awaits deletion | `400 engine_attached_to_queues_waiting_for_deletion` |
| a missing or wrong token | `401` |

### E. Scenario wrappers — the mechanical refactor

Each scenario body is extracted unchanged into a function over `&LiveConfig`,
and gains a second wrapper:

```rust
async fn round_trip_core(cfg: &LiveConfig) { /* today's body, verbatim */ }

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_round_trip_core() {
    let fake = FakeOrg::start().await;
    round_trip_core(&fake.config()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_round_trip_core() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    round_trip_core(&cfg).await;
}
```

The live wrapper keeps `#[ignore]` and the same `from_env()` gate, so live
behavior is bit-for-bit what it is today.

### F. Promotion offline

`FakeOrg::start_pair()` plus `ProjectFixture::init_envs` gives two orgs with
different ids and tokens — the shape `LiveConfig::target` documents as the only
honest way to test a promotion (`tests/live/support/config.rs:23`). Today
those scenarios require a second real org; after this they run in `cargo test`,
which is the first offline coverage the promotion chain has ever had.

### G. Staging

- **Stage 1 — the instrument.** `FakeOrg`, `OrgState`, `Quirks` and strict
  validation for the `core`-tagged kinds (label, workspace, schema, queue,
  inbox, hook, rule), the queue's auto-created email templates, the async
  queue-delete state machine, and `fake_round_trip_core` green against the
  existing `testdata/live/expected/round_trip.toml` golden. Exactly **one**
  scenario is extracted per §E in this stage — `round_trip_core`. Every other
  scenario file is untouched, so stage 1 cannot regress the live suite.
- **Stage 2 — port the remaining non-MDH scenarios**: collisions, cross_refs,
  sidecars, engines, email_templates, saved_views, organization,
  conflicts_deletes, deploy_flow, janitor, ordering, server_truth, cli_surface,
  migrate_promotion. Run the live suite once against the sandbox first, to
  prove the extraction changed nothing.
- **Stage 3 — the matrix and the fuzzer.** Per-kind lifecycle across every
  `PUSH_CAPABLE` kind (create → converge, local edit → converge, remote edit →
  converge, delete → converge), then `proptest` (already a dev-dependency) over
  edit sequences. Stages 1–2 preserve known facts; stage 3 is where unknown
  churn is found.

## Backward compatibility

- **No production-code changes.** This is test infrastructure only.
- `cargo test --test live -- --ignored` still selects exactly the live set,
  because `fake_*` tests are not `#[ignore]`d and `--ignored` excludes them.
  The documented commands at `README.md:522`, `:530` and `:601` keep working
  unchanged, including `RDC_LIVE_CAPTURE=1`.
- The 108 stateless-wiremock tests in `tests/cli_sync.rs` are untouched.
- Goldens are shared, not forked: `CapturedState` is already id-free.
- **CI behavior does change**, deliberately: `cargo test --locked` is the
  weekly release gate (`.github/workflows/weekly-release.yaml:141`), so from
  stage 1 the release is gated on convergence. The cost of that is that a flaky
  fake blocks a release — see Failure modes.

## Testing

- `fake_round_trip_core` asserts convergence twice — after the initial pull and
  after the label push — via the existing `assert_converged`.
- `testdata/live/snapshot/**` is a hand-written INPUT fixture, not an
  expected-output oracle: it is not seeded through the API and pulled, but
  written straight to disk with no lockfile entries, so `rdc sync` classifies
  every object as a `LocalCreate` and POSTs the whole graph
  (`tests/live/support/snapshot.rs:1-12`). No expected-pulled-tree fixture
  exists yet, byte-level or otherwise — a field-level oracle for what a pull
  must produce is unbuilt work. `testdata/live/expected/*.toml` are the
  run-agnostic goldens both backends share.
- The provenance guard test (§C) covers the quirk registry.
- `OrgState` gets direct unit tests for pagination (including the
  `total_pages == 0` fallback), id/url minting, the logical clock, and each
  rejection in §D.
- A disagreement between `fake_x` and `live_x` is a first-class defect with
  exactly two resolutions: the fake's model is wrong (fix the quirk, update its
  citation) or `rdc` is wrong (fix `rdc`). **Weakening the scenario is not a
  resolution.**

## Failure modes

- **The fake drifts into a second implementation.** Bounded by modelling only
  the 28 endpoints `rdc` calls and only the fields it reads or writes, and by
  the provenance guard refusing unprovable quirks.
- **False confidence from a wrong model.** Bounded by dual-backend scenarios
  and the run-agnostic goldens at `testdata/live/expected/*.toml`.
- **A flaky fake blocks the weekly release.** Bounded by per-test instances, a
  logical clock instead of the wall clock, zero sleeps, and no randomness
  beyond the existing `RunId`.
- **The extraction silently breaks the live suite.** Bounded by keeping the
  live wrapper's `#[ignore]` and gate identical, and by one live run against
  the sandbox before stage 2.

## Out of scope

- **MDH / Data Storage.** Its 14 methods include `aggregate` with real
  pipelines, search-index materialization and Mongo semantics. The fake answers
  its endpoints 404, which `rdc` already tolerates, so an MDH-less env is
  modelled honestly rather than stubbed. The 3 MDH scenarios stay live-only; a
  fake for them is a separate design.
- **Latency-bound behavior**: retry/backoff, real 429 handling, rate-limit
  pacing, materialization waits. Live-only by decision 4.
- **Discovering new server facts.** The fake preserves facts already paid for;
  it converts regressions of known facts from live-only-detectable to
  CI-detectable. New behavior still arrives from live runs, and this design
  does not retire the live suite.
- **The two sibling workstreams** this diagnosis also identified, deliberately
  deferred so the instrument exists before either is attempted: collapsing the
  write-back law into one enforced path with the post-write authority owned by
  the kind registry, and replacing push's fail-fast with dependency-aware
  per-object isolation plus an end-of-cycle convergence report.
