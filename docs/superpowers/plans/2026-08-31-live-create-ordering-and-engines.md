# Live create-ordering and engine coverage — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the live suite an explicit, server-backed assertion of the order
`rdc sync` creates and deletes objects in, and first-ever live coverage of the
`engines` / `engine_fields` kinds.

**Architecture:** Two new live scenarios plus two new support modules. A trace
module parses the CSV that `RDC_TRACE_HTTP` already writes, turning request
order into something a test can assert. A hand-authored snapshot fixture lets a
single-org run exercise rdc's *create* path — today only reachable with a second
org. No `src/` changes: every API method and the trace itself already exist.

**Tech Stack:** Rust, `tokio::test`, `assert_cmd`, the existing
`tests/live/support/*` harness.

**Spec:** `docs/superpowers/specs/2026-08-31-live-create-ordering-and-engines-design.md`

## Global Constraints

- **No customer names or customer-specific identifiers** anywhere — source,
  tests, fixtures, docs, commit messages. Placeholders only (`acme`, `invoices`,
  `test`/`dev`/`prod`). Fixture names all carry the `rdc-it-<run>-` marker.
- **Batch edits, compile once.** rdc compiles slowly; make every edit for a task,
  then run one `cargo test`. Do not compile per TDD step.
- **Never run repo-wide `cargo fmt`** — this repo is not fmt-clean under the
  local rustfmt and a fmt-check failure is pre-existing, not a regression.
- **Commit straight to local `main`**; never `git push`. Never park work on a
  `fix/` or `work/` branch.
- Live tests are `#[ignore]`, opt-in through `RDC_LIVE_*`. Support-module logic
  gets hermetic unit tests that run under a plain `cargo test`.
- Live sandbox tokens expire in ~30 minutes. A burst of live failures is usually
  a 401 — check with `curl` before debugging.
- Run live tests with `--test-threads=1`.

**Live env for every live step below:**

```sh
export RDC_LIVE_API_BASE="https://api.elis.rossum.ai/v1"
export RDC_LIVE_ORG_ID="214757"
export RDC_LIVE_TOKEN="<current sandbox token>"
```

---

### Task 1: The trace module

Parses the CSV `RDC_TRACE_HTTP` writes so a scenario can assert request order.

**Files:**
- Create: `tests/live/support/trace.rs`
- Modify: `tests/live/support/mod.rs` (add `pub mod trace;`)

**Interfaces:**
- Consumes: nothing.
- Produces: `Trace::read(&Path) -> Trace`, `Trace::parse(&str) -> Trace`,
  `Trace::first(&self, method: &str, endpoint: &str) -> Option<usize>`,
  `Trace::last(...) -> Option<usize>`,
  `Trace::assert_before(&self, a: (&str, &str), b: (&str, &str), why: &str)`,
  `Trace::is_empty(&self) -> bool`, and the struct
  `TraceLine { status: String, method: String, url: String }`.

- [ ] **Step 1: Write the module with its failing tests**

Create `tests/live/support/trace.rs`:

```rust
//! Parse the `RDC_TRACE_HTTP` CSV so a scenario can assert the ORDER in which
//! `rdc` issued its requests.
//!
//! `src/api/retry.rs` writes one line per HTTP **attempt**, from the single
//! `send_once` chokepoint both the core API client and the Data Storage client
//! funnel through:
//!
//! ```text
//! epoch_ms,limiter_wait_ms,duration_ms,status,desc
//! 1788174659806.4,0.0,371.1,200,GET https://api.elis.rossum.ai/v1/workspaces?page=1
//! ```
//!
//! Two properties of that format drive the parser. `desc` is LAST and unquoted
//! and may itself contain commas, so a row splits on the first four separators
//! only. And because both clients share the sink, a run's file also holds
//! `POST https://elis.rossum.ai/svc/data-storage/...` lines from MDH — which
//! carry their own `/v1/` segment and would otherwise be mistaken for core-API
//! calls. [`Trace::endpoint_of`] rejects them explicitly.
//!
//! # Why last-of-A vs first-of-B is a sound ordering test
//!
//! `push::push_classified` awaits its per-kind drivers one after another, so
//! requests of two different kinds can never interleave, however much
//! concurrency a single driver uses internally (`push/concurrent.rs`).
//! Comparing the LAST index of kind A against the FIRST index of kind B is
//! therefore exact, and it is also immune to a retried attempt appearing in the
//! file twice.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceLine {
    pub status: String,
    pub method: String,
    pub url: String,
}

#[derive(Debug, Clone, Default)]
pub struct Trace {
    pub lines: Vec<TraceLine>,
}

#[allow(dead_code)]
impl Trace {
    /// Read and parse a trace file. A missing file is an EMPTY trace, not an
    /// error: a command that issued no request writes nothing at all.
    pub fn read(path: &Path) -> Trace {
        match std::fs::read_to_string(path) {
            Ok(raw) => Trace::parse(&raw),
            Err(_) => Trace::default(),
        }
    }

    pub fn parse(raw: &str) -> Trace {
        let mut lines = Vec::new();
        for row in raw.lines() {
            // epoch,limiter_wait,duration,status,desc — `desc` is last and may
            // contain commas, so split on the first four separators only.
            let mut it = row.splitn(5, ',');
            let (_epoch, _wait, _dur) = (it.next(), it.next(), it.next());
            let (Some(status), Some(desc)) = (it.next(), it.next()) else {
                continue;
            };
            let Some((method, url)) = desc.split_once(' ') else { continue };
            lines.push(TraceLine {
                status: status.to_string(),
                method: method.to_string(),
                url: url.to_string(),
            });
        }
        Trace { lines }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The core-API endpoint a url addresses: the first path segment after
    /// `/v1/`, with any query string removed. `None` for a Data Storage url,
    /// which carries its own `/v1/` but is a different service.
    fn endpoint_of(url: &str) -> Option<&str> {
        if url.contains("/svc/data-storage/") {
            return None;
        }
        let after = url.split("/v1/").nth(1)?;
        let seg = after.split(['/', '?']).next()?;
        (!seg.is_empty()).then_some(seg)
    }

    fn positions(&self, method: &str, endpoint: &str) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.method == method && Trace::endpoint_of(&l.url) == Some(endpoint))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn first(&self, method: &str, endpoint: &str) -> Option<usize> {
        self.positions(method, endpoint).first().copied()
    }

    pub fn last(&self, method: &str, endpoint: &str) -> Option<usize> {
        self.positions(method, endpoint).last().copied()
    }

    /// Render the trace rows in `lo..=hi` (clamped), one per line, for a
    /// failure message that says what actually ran.
    fn window(&self, lo: usize, hi: usize) -> String {
        let lo = lo.saturating_sub(2);
        let hi = (hi + 2).min(self.lines.len().saturating_sub(1));
        self.lines[lo..=hi]
            .iter()
            .enumerate()
            .map(|(n, l)| format!("  #{:<4} {} {} {}", lo + n, l.status, l.method, l.url))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Assert every `a` request precedes every `b` request. Both must have
    /// occurred — a missing one is a failure, not a vacuous pass, because the
    /// commonest way for an ordering test to rot is for the request to stop
    /// being made at all.
    pub fn assert_before(&self, a: (&str, &str), b: (&str, &str), why: &str) {
        let last_a = self.last(a.0, a.1).unwrap_or_else(|| {
            panic!(
                "ordering: no `{} /{}` in the trace, so `{} /{}` -> `{} /{}` could not be \
                 checked ({why}).\nTrace:\n{}",
                a.0, a.1, a.0, a.1, b.0, b.1,
                self.window(0, self.lines.len().saturating_sub(1))
            )
        });
        let first_b = self.first(b.0, b.1).unwrap_or_else(|| {
            panic!(
                "ordering: no `{} /{}` in the trace, so `{} /{}` -> `{} /{}` could not be \
                 checked ({why}).\nTrace:\n{}",
                b.0, b.1, a.0, a.1, b.0, b.1,
                self.window(0, self.lines.len().saturating_sub(1))
            )
        });
        assert!(
            last_a < first_b,
            "ordering violated: the last `{} /{}` is at #{last_a} but the first `{} /{}` is \
             at #{first_b} — {why}.\nTrace around the violation:\n{}",
            a.0, a.1, b.0, b.1,
            self.window(first_b.min(last_a), first_b.max(last_a))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trace with the shape `src/api/retry.rs` really writes, including a
    /// Data Storage line and a `desc` containing a comma-bearing query string.
    const SAMPLE: &str = "\
1788174659802.8,0.0,367.6,200,GET https://api.elis.rossum.ai/v1/organizations/214757
1788174659838.5,0.0,404.0,200,POST https://elis.rossum.ai/svc/data-storage/api/v1/collections/list
1788174659900.0,0.0,110.0,201,POST https://api.elis.rossum.ai/v1/engines
1788174659950.0,0.0,110.0,201,POST https://api.elis.rossum.ai/v1/engine_fields
1788174660000.0,0.0,110.0,201,POST https://api.elis.rossum.ai/v1/schemas
1788174660100.0,0.0,110.0,201,POST https://api.elis.rossum.ai/v1/queues
1788174660200.0,0.0,110.0,200,GET https://api.elis.rossum.ai/v1/queues?page_size=100,ordering=id
";

    #[test]
    fn parses_method_status_and_url() {
        let t = Trace::parse(SAMPLE);
        assert_eq!(t.lines.len(), 7);
        assert_eq!(t.lines[2].method, "POST");
        assert_eq!(t.lines[2].status, "201");
        assert_eq!(t.lines[2].url, "https://api.elis.rossum.ai/v1/engines");
    }

    /// `desc` is unquoted and may contain commas; splitting on all of them
    /// would truncate the url.
    #[test]
    fn a_comma_inside_desc_does_not_truncate_the_url() {
        let t = Trace::parse(SAMPLE);
        assert_eq!(t.lines[6].url, "https://api.elis.rossum.ai/v1/queues?page_size=100,ordering=id");
    }

    /// Data Storage shares the sink and carries its own `/v1/`. Mistaking it
    /// for a core-API call would make MDH traffic pollute every assertion.
    #[test]
    fn data_storage_urls_are_not_core_api_endpoints() {
        let t = Trace::parse(SAMPLE);
        assert_eq!(t.positions("POST", "collections"), Vec::<usize>::new());
    }

    #[test]
    fn endpoint_ignores_the_id_and_the_query() {
        assert_eq!(
            Trace::endpoint_of("https://api.elis.rossum.ai/v1/queues/4135179"),
            Some("queues")
        );
        assert_eq!(
            Trace::endpoint_of("https://api.elis.rossum.ai/v1/queues?page=1"),
            Some("queues")
        );
    }

    #[test]
    fn assert_before_passes_on_correct_order() {
        Trace::parse(SAMPLE).assert_before(
            ("POST", "engine_fields"),
            ("POST", "queues"),
            "engine fields must exist before a queue binds the engine",
        );
    }

    #[test]
    #[should_panic(expected = "ordering violated")]
    fn assert_before_fails_on_reversed_order() {
        Trace::parse(SAMPLE).assert_before(
            ("POST", "queues"),
            ("POST", "engines"),
            "deliberately backwards",
        );
    }

    /// A request that stopped being made must fail loudly rather than pass by
    /// checking nothing — the commonest way an ordering test rots.
    #[test]
    #[should_panic(expected = "no `POST /labels`")]
    fn assert_before_fails_when_a_request_is_absent() {
        Trace::parse(SAMPLE).assert_before(
            ("POST", "labels"),
            ("POST", "rules"),
            "absent on both sides",
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_trace() {
        assert!(Trace::read(std::path::Path::new("/nonexistent/trace.csv")).is_empty());
    }
}
```

Add to `tests/live/support/mod.rs`, keeping the file's existing order:

```rust
pub mod trace;
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --test live trace:: -- --nocapture`
Expected: 8 passed. (These are hermetic — no `--ignored`, no network.)

- [ ] **Step 3: Commit**

```bash
git add tests/live/support/trace.rs tests/live/support/mod.rs
git commit -m "test(live): parse the RDC_TRACE_HTTP csv so order can be asserted"
```

---

### Task 2: `run_rdc_traced`

**Files:**
- Modify: `tests/live/support/project.rs`

**Interfaces:**
- Consumes: `support::trace::Trace` from Task 1.
- Produces: `ProjectFixture::run_rdc_traced(&self, args: &[&str]) -> (std::process::Output, Trace)`.

- [ ] **Step 1: Add the method**

At the top of `tests/live/support/project.rs`, extend the imports:

```rust
use crate::support::trace::Trace;
use std::sync::atomic::{AtomicU32, Ordering};
```

Add a counter field to the struct and initialise it in both constructors:

```rust
pub struct ProjectFixture {
    dir: TempDir,
    runs: AtomicU32,
}
```

In `init_envs`, change the final `Ok(ProjectFixture { dir })` to:

```rust
        Ok(ProjectFixture { dir, runs: AtomicU32::new(0) })
```

Then add the method next to `run_rdc`:

```rust
    /// `run_rdc`, with `RDC_TRACE_HTTP` pointed at a fresh file so the caller
    /// can assert the ORDER of the requests this invocation made.
    ///
    /// The trace lands at the PROJECT ROOT, deliberately outside every root
    /// `converge::tracked_roots` captures (`envs/<env>`, `.rdc/state/<env>.base`,
    /// `.rdc/conflicts/<env>`), so tracing a run can never itself perturb a
    /// convergence assertion. One file per invocation, because the sink in
    /// `api::retry` is a process-wide `OnceLock` initialised from the
    /// environment on the first request — a second `rdc` process needs a second
    /// path, and appending both runs to one file would make indices meaningless.
    #[allow(dead_code)]
    pub fn run_rdc_traced(&self, args: &[&str]) -> (Output, Trace) {
        let n = self.runs.fetch_add(1, Ordering::SeqCst);
        let trace_path = self.dir.path().join(format!(".rdc-trace-{n}.csv"));
        let out = assert_cmd::Command::cargo_bin("rdc")
            .unwrap()
            .current_dir(self.dir.path())
            .env("RDC_TRACE_HTTP", &trace_path)
            .args(args)
            .output()
            .expect("spawning rdc");
        (out, Trace::read(&trace_path))
    }
```

- [ ] **Step 2: Add a hermetic test**

Append to the existing `mod tests` in `project.rs`:

```rust
    /// Hermetic: `--help` makes no HTTP request, so the trace is empty — which
    /// is exactly what proves the env-var plumbing does not break an ordinary
    /// invocation. The real exercise is in the live ordering scenario.
    #[test]
    fn run_rdc_traced_returns_output_and_an_empty_trace_for_a_networkless_command() {
        let cfg = LiveConfig {
            api_base: "https://example.rossum.app/api/v1".into(),
            org_id: 999,
            token: "tok".into(),
            target: None,
        };
        let p = ProjectFixture::init(&cfg, &["test"]).unwrap();
        let (out, trace) = p.run_rdc_traced(&["--help"]);
        assert!(out.status.success(), "rdc --help failed");
        assert!(trace.is_empty(), "a networkless command must trace nothing: {trace:?}");
    }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --test live project:: -- --nocapture`
Expected: 3 passed (the two existing `init_*` tests plus the new one).

- [ ] **Step 4: Commit**

```bash
git add tests/live/support/project.rs
git commit -m "test(live): run_rdc_traced, so a scenario can read back request order"
```

---

### Task 3: Engines in the live client, teardown and janitor

**Files:**
- Modify: `tests/live/support/client.rs`
- Modify: `tests/live/support/teardown.rs`
- Modify: `tests/live/scenarios/janitor.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `LiveClient::create("engine" | "engine_field", …)`,
  `LiveClient::delete("engine" | "engine_field", id)`,
  `LiveClient::list_ids_by_name_prefix("engine" | "engine_field", prefix)`, and
  a `teardown_by_prefix` that sweeps both kinds.

- [ ] **Step 1: Extend the client kind maps**

In `create`, before the `other =>` arm:

```rust
            "engine" => {
                let e = self.inner.create_engine(body, None).await?;
                (e.id, e.url)
            }
            "engine_field" => {
                let f = self.inner.create_engine_field(body, None).await?;
                (f.id, f.url)
            }
```

In `delete`, before the `other =>` arm:

```rust
            "engine" => self.inner.delete_engine(id, None).await,
            "engine_field" => self.inner.delete_engine_field(id, None).await,
```

In `list_ids_by_name_prefix`, before the `other =>` arm:

```rust
            "engine" => to_values(self.inner.list_engines(None).await?)?,
            // Engine field `name` must equal the schema datapoint id it covers.
            // Hyphens are legal there (verified against the API), so a field
            // this harness creates carries the run marker in its own name and
            // is prefix-matchable exactly like every other kind.
            "engine_field" => to_values(self.inner.list_engine_fields(None).await?)?,
```

- [ ] **Step 2: Extend teardown**

In `tests/live/support/teardown.rs`, insert this block AFTER the schema-delete
loop and BEFORE the `// Parents last.` workspace/label loop:

```rust
    // Engine fields, then engines — after queues and schemas, and the two
    // halves are here for different reasons.
    //
    // The FIELD sweep genuinely benefits from the position: `DELETE
    // /engine_fields/<id>` answers `409 conflict_referenced` ("Cannot delete
    // engine field used in a schema") while the schema that its name covers is
    // still around, and that clears once the schema above is gone.
    //
    // The ENGINE sweep cannot be helped by any ordering. An engine that was
    // ever bound to a queue is refused with `400
    // engine_attached_to_active_queues` while the queue lives, and then with
    // `400 engine_attached_to_queues_waiting_for_deletion` — "after up to 24
    // hours" — for as long as the queue is draining. `DELETE /queues` returns
    // `202 deletion_requested`, so the queue is never actually gone by the time
    // this runs. Best-effort on purpose: log and leave it, and a later run's
    // janitor collects it. There is no retry loop because the window is a day,
    // not the fifteen seconds `delete_schema_with_retry` waits out.
    for kind in ["engine_field", "engine"] {
        let found = match client.list_ids_by_name_prefix(kind, prefix).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        for (id, name) in found {
            if let Err(e) = client.delete(kind, id).await {
                eprintln!("teardown: delete {kind} {id} ({name}) failed (continuing): {e:#}");
            }
        }
    }
```

- [ ] **Step 3: Teach the janitor about the asymmetry**

In `tests/live/scenarios/janitor.rs`, the final assertion loop lists kinds that
MUST be fully gone. Engines cannot join it. Replace the source-org loop's
trailing comment and add an explicit non-assertion below it:

```rust
    // Synchronously-deletable kinds MUST be fully gone after the sweep.
    for kind in ["workspace", "hook", "label", "rule", "inbox", "saved_view"] {
        let left = client.list_ids_by_name_prefix(kind, RunId::marker()).await.unwrap_or_default();
        assert!(left.is_empty(), "janitor left {kind} objects: {left:?}");
    }

    // Engines are deliberately NOT asserted empty. One that was bound to a
    // queue is undeletable until that queue finishes purging — up to 24 hours —
    // so a sweep run soon after `live_push_create_ordering` legitimately leaves
    // one behind, and it goes on the next run. Report the backlog instead, so a
    // number that keeps climbing is visible rather than silent.
    let engines_left = client
        .list_ids_by_name_prefix("engine", RunId::marker())
        .await
        .unwrap_or_default();
    if !engines_left.is_empty() {
        eprintln!(
            "janitor: {} harness engine(s) still pending their queues' purge (expected; \
             they go on a later run): {:?}",
            engines_left.len(),
            engines_left
        );
    }
```

- [ ] **Step 4: Compile and run the hermetic suite**

Run: `cargo test --test live 2>&1 | tail -20`
Expected: the hermetic support tests pass; the live scenarios report as ignored.

- [ ] **Step 5: Run the janitor live to prove the new sweep works**

Run: `cargo test --test live live_janitor_sweep -- --ignored --nocapture 2>&1 | tail -30`
Expected: PASS. It should print the pending-engine line for the leftover
`rdc-it-probe*` engines from the spec's probes if their queues have not purged
yet, and delete them if they have.

- [ ] **Step 6: Commit**

```bash
git add tests/live/support/client.rs tests/live/support/teardown.rs tests/live/scenarios/janitor.rs
git commit -m "test(live): sweep engines and engine fields, and say why engines may linger"
```

---

### Task 4: The engine lifecycle scenario

Self-cleaning by construction: it never binds a queue, so nothing it creates is
subject to the 24-hour rule.

**Files:**
- Create: `tests/live/scenarios/engines.rs`
- Modify: `tests/live/scenarios/mod.rs` (add `pub mod engines;` in alphabetical position, after `deploy_flow`)

**Interfaces:**
- Consumes: `LiveClient::create("engine"|"engine_field", …)` from Task 3.
- Produces: nothing later tasks depend on.

- [ ] **Step 1: Write the scenario**

Create `tests/live/scenarios/engines.rs`:

```rust
use crate::support::assert_local::load_lockfile;
use crate::support::assert_remote::assert_remote_field;
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::teardown::Teardown;

/// Full `engines` / `engine_fields` lifecycle against a real org: pull
/// round-trip, local edit pushed, a field CREATED from a hand-written file, a
/// field DELETED through a tombstone, then the engine itself deleted.
///
/// This scenario deliberately **never binds a queue to the engine**, and that
/// is the whole reason it can run on every live run. A bound engine is refused
/// deletion with `400 engine_attached_to_active_queues` while its queue lives,
/// and then `400 engine_attached_to_queues_waiting_for_deletion` — "after up to
/// 24 hours" — while the queue drains; nulling `queue.engine` to escape is
/// refused too ("Queue does not have an engine"). Unbound, the same objects
/// delete cleanly with `204`. Queue binding is covered by
/// `live_push_create_ordering`, which pays that price once, on purpose.
///
/// Note the field slug: `engine_fields` are keyed in the lockfile by the
/// COMPOUND `<engine-slug>/<field-slug>`, and live on disk under
/// `engines/<engine>/fields/<field>.json`. A field's `name` must match the
/// schema datapoint id it covers, so it is snake_case where its slug is
/// hyphenated — `amount_due` becomes `amount-due.json`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_engines_round_trip() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    // --- seed: one engine, two fields, no queue anywhere near it ---
    let engine_name = run_id.prefix("engine");
    let (engine_id, engine_url) = client
        .create(
            "engine",
            &serde_json::json!({
                "name": engine_name,
                "type": "extractor",
                "learning_enabled": false,
                "description": "seeded",
            }),
        )
        .await
        .expect("create engine");

    let field_name = format!("{}amount_due", run_id.list_prefix());
    let (field_id, _) = client
        .create(
            "engine_field",
            &serde_json::json!({
                "engine": engine_url,
                "name": field_name,
                "label": "Amount Due",
                "type": "number",
                "subtype": "amount",
            }),
        )
        .await
        .expect("create engine field");

    // --- pull ---
    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    let pull = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(pull.status.success(), "pull failed: {}", combined(&pull));

    let prefix = run_id.list_prefix();
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let engine_slug = lf
        .slug_for_id("engines", engine_id)
        .expect("the seeded engine must be tracked")
        .to_string();
    let field_slug = lf
        .slug_for_id("engine_fields", field_id)
        .expect("the seeded engine field must be tracked")
        .to_string();

    // The compound key is the point: a flat field slug would collide across
    // engines that both define, say, `amount_due`.
    assert_eq!(
        field_slug,
        format!("{engine_slug}/{}amount-due", prefix),
        "engine fields must be keyed by <engine>/<field>"
    );

    let engine_rel = format!("envs/test/engines/{engine_slug}/engine.json");
    let field_rel = format!("envs/test/engines/{engine_slug}/fields/{}amount-due.json", prefix);
    assert!(project.exists(&engine_rel), "missing {engine_rel}");
    assert!(project.exists(&field_rel), "missing {field_rel}");

    let on_disk_field = project.read_json(&field_rel);
    assert_eq!(
        on_disk_field["engine"],
        serde_json::json!(format!("rdc://engines/{engine_slug}")),
        "a field's engine ref must be portable on disk"
    );

    assert_converged(&project, "test", &prefix, "after the initial pull");

    // --- edit both, push, confirm the remote took it ---
    let mut engine = project.read_json(&engine_rel);
    engine["description"] = serde_json::json!("edited by rdc");
    project.write_json(&engine_rel, &engine);

    let mut field = project.read_json(&field_rel);
    field["label"] = serde_json::json!("Amount Due (edited)");
    project.write_json(&field_rel, &field);

    let push = project.run_rdc(&["sync", "test"]);
    assert!(push.status.success(), "push failed: {}", combined(&push));

    assert_remote_field(
        &client,
        "engine",
        engine_id,
        "description",
        &serde_json::json!("edited by rdc"),
    )
    .await
    .expect("the engine edit must reach the remote");
    assert_remote_field(
        &client,
        "engine_field",
        field_id,
        "label",
        &serde_json::json!("Amount Due (edited)"),
    )
    .await
    .expect("the field edit must reach the remote");

    assert_converged(&project, "test", &prefix, "after pushing engine + field edits");

    // --- CREATE a second field from a hand-written file ---
    let new_rel = format!("envs/test/engines/{engine_slug}/fields/{}amount-tax.json", prefix);
    project.write_json(
        &new_rel,
        &serde_json::json!({
            "id": 0,
            "url": "",
            "name": format!("{prefix}amount_tax"),
            "engine": format!("rdc://engines/{engine_slug}"),
            "label": "Amount Tax",
            "type": "number",
            "subtype": "amount",
            "pre_trained_field_id": null,
            "tabular": false,
            "multiline": "false",
        }),
    );

    let create = project.run_rdc(&["sync", "test"]);
    assert!(create.status.success(), "field create failed: {}", combined(&create));

    let lf = load_lockfile(project.path(), "test").expect("lockfile after create");
    let new_id = lf
        .objects
        .get("engine_fields")
        .and_then(|m| m.get(&format!("{engine_slug}/{}amount-tax", prefix)))
        .unwrap_or_else(|| panic!("the created field must be in the lockfile"))
        .id;
    assert!(
        client.find_listed_value("engine_field", new_id).await.expect("list").is_some(),
        "the created field must exist remotely (id {new_id})"
    );

    assert_converged(&project, "test", &prefix, "after creating an engine field");

    // --- DELETE that field through a tombstone ---
    std::fs::remove_file(project.path().join(&new_rel)).expect("removing the field file");
    let del = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(del.status.success(), "field delete failed: {}", combined(&del));
    assert!(
        client.find_listed_value("engine_field", new_id).await.expect("list").is_none(),
        "the tombstoned field must be gone remotely (id {new_id})"
    );

    assert_converged(&project, "test", &prefix, "after deleting an engine field");

    // --- DELETE the engine, with its remaining field ---
    std::fs::remove_dir_all(project.path().join(format!("envs/test/engines/{engine_slug}")))
        .expect("removing the engine dir");
    let del_engine = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(del_engine.status.success(), "engine delete failed: {}", combined(&del_engine));
    assert!(
        client.find_listed_value("engine", engine_id).await.expect("list").is_none(),
        "an UNBOUND engine must delete cleanly (id {engine_id}); if this fails, something \
         bound it to a queue and the 24h rule now applies"
    );

    drop(teardown);
}
```

- [ ] **Step 2: Add the `write_json` helper the scenario uses**

`ProjectFixture` has `read_json` but no writer. Add it next to `read_json` in
`tests/live/support/project.rs`:

```rust
    /// Write pretty JSON with a trailing newline — the exact byte shape rdc's
    /// codec writes, so a hand-authored file does not read as drift the moment
    /// it lands.
    #[allow(dead_code)]
    pub fn write_json(&self, rel: &str, v: &serde_json::Value) {
        let path = self.dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|e| panic!("creating {}: {e}", parent.display()));
        }
        let mut bytes = serde_json::to_vec_pretty(v).expect("serialising json");
        bytes.push(b'\n');
        std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("writing {rel}: {e}"));
    }
```

- [ ] **Step 3: Register the module**

In `tests/live/scenarios/mod.rs`, add after `pub mod deploy_flow;`:

```rust
pub mod engines;
```

- [ ] **Step 4: Compile**

Run: `cargo test --test live --no-run 2>&1 | tail -20`
Expected: compiles clean. Fix any name mismatch against `LiveClient` /
`Lockfile` helpers (`slug_for_id`, `find_listed_value`) before moving on.

- [ ] **Step 5: Run it live**

Run: `cargo test --test live live_engines_round_trip -- --ignored --nocapture --test-threads=1 2>&1 | tail -40`
Expected: PASS.

If the pull writes engine fields rdc's codec normalises differently from the
hand-written create body (a spurious diff at the `assert_converged` after the
create), align the fixture with what a pull actually produces:

```sh
python3 -m json.tool "$(find /tmp -path '*engines/*/fields/*.json' | head -1)"
```

- [ ] **Step 6: Verify nothing leaked**

```bash
curl -s -H "Authorization: Bearer $RDC_LIVE_TOKEN" \
  "$RDC_LIVE_API_BASE/engines?page_size=100" \
  | python3 -c "import json,sys; print([e['name'] for e in json.load(sys.stdin)['results'] if 'rdc-it-' in e['name']])"
```

Expected: `[]` (or only leftovers from other runs — none from this one).

- [ ] **Step 7: Commit**

```bash
git add tests/live/scenarios/engines.rs tests/live/scenarios/mod.rs tests/live/support/project.rs
git commit -m "test(live): engine and engine-field lifecycle, unbound so it self-cleans"
```

---

### Task 5: The hand-authored create snapshot

**Files:**
- Create: `testdata/live/snapshot/` (the tree listed below)
- Create: `tests/live/support/snapshot.rs`
- Modify: `tests/live/support/mod.rs` (add `pub mod snapshot;`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `snapshot::write_snapshot(project: &ProjectFixture, env: &str, run_id: &RunId, org_url: &str)`,
  and `snapshot::substitute(raw: &str, run: &str, org_url: &str) -> String`.

**Placeholders.** `{{RUN}}` becomes `run_id.as_str()`, so a fixture path reads
`rdc-it-{{RUN}}-engine` and expands to `rdc-it-<id>-engine` — the marker stays
visible in the fixture. `{{ORG_URL}}` becomes the env's organization URL, needed
because `workspace.organization` is required on create and is env-specific.

- [ ] **Step 1: Author the fixture tree**

All eleven files below live under `testdata/live/snapshot/`. Every one is
authored the way rdc authors a create: `"id": 0`, `"url": ""`, cross-refs as
`rdc://`. Names equal slugs throughout so teardown's name-prefix match and the
convergence snapshot's path-prefix match see the same string.

`engines/rdc-it-{{RUN}}-engine/engine.json`:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-engine",
  "type": "extractor",
  "learning_enabled": false,
  "description": "",
  "settings": {}
}
```

`engines/rdc-it-{{RUN}}-engine/fields/rdc-it-{{RUN}}-probe-field.json` — the
`name` must equal the schema datapoint id below, exactly:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-probe_field",
  "engine": "rdc://engines/rdc-it-{{RUN}}-engine",
  "label": "Probe Field",
  "type": "string",
  "subtype": "alphanumeric",
  "pre_trained_field_id": null,
  "tabular": false,
  "multiline": "false"
}
```

`labels/rdc-it-{{RUN}}-priority.json`:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-priority",
  "organization": "{{ORG_URL}}",
  "color": "#ff8800"
}
```

`workspaces/rdc-it-{{RUN}}-ws/workspace.json`:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-ws",
  "organization": "{{ORG_URL}}",
  "queues": []
}
```

`workspaces/rdc-it-{{RUN}}-ws/queues/rdc-it-{{RUN}}-invoices/schema.json` — one
extracted datapoint, so exactly one engine field covers it:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-invoices",
  "queues": [],
  "content": [
    {
      "category": "section",
      "id": "header",
      "label": "Header",
      "children": [
        {
          "category": "datapoint",
          "id": "rdc-it-{{RUN}}-probe_field",
          "label": "Probe Field",
          "type": "string"
        }
      ]
    }
  ]
}
```

`workspaces/rdc-it-{{RUN}}-ws/queues/rdc-it-{{RUN}}-invoices/queue.json` — the
`engine` binding is what makes the server enforce push order:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-invoices",
  "workspace": "rdc://workspaces/rdc-it-{{RUN}}-ws",
  "schema": "rdc://schemas/rdc-it-{{RUN}}-invoices",
  "engine": "rdc://engines/rdc-it-{{RUN}}-engine",
  "hooks": [],
  "rules": [],
  "webhooks": [],
  "users": [],
  "locale": "en_GB"
}
```

`workspaces/rdc-it-{{RUN}}-ws/queues/rdc-it-{{RUN}}-invoices/inbox.json` —
`email_prefix` is mandatory on create and forms a globally unique address, so it
carries the run id:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-invoices",
  "queues": ["rdc://queues/rdc-it-{{RUN}}-invoices"],
  "email_prefix": "rdc-it-{{RUN}}-invoices"
}
```

`workspaces/rdc-it-{{RUN}}-ws/queues/rdc-it-{{RUN}}-invoices/email-templates/rdc-it-{{RUN}}-notice.json`:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-notice",
  "queue": "rdc://queues/rdc-it-{{RUN}}-invoices",
  "type": "custom",
  "subject": "Document received",
  "message": "<p>Please review the attached document.</p>",
  "automate": false
}
```

`hooks/rdc-it-{{RUN}}-validator.json` plus its code sidecar:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-validator",
  "type": "function",
  "events": ["annotation_content"],
  "queues": ["rdc://queues/rdc-it-{{RUN}}-invoices"],
  "active": true,
  "config": { "runtime": "python3.12" }
}
```

`hooks/rdc-it-{{RUN}}-validator.py`:
```python
def rossum_hook_request_handler(payload):
    return {}
```

`hooks/rdc-it-{{RUN}}-post-validator.json` — `run_after` is the edge that forces
the deferred relink, because at create time the validator has no URL yet:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-post-validator",
  "type": "function",
  "events": ["annotation_content"],
  "queues": ["rdc://queues/rdc-it-{{RUN}}-invoices"],
  "run_after": ["rdc://hooks/rdc-it-{{RUN}}-validator"],
  "active": true,
  "config": { "runtime": "python3.12" }
}
```

`hooks/rdc-it-{{RUN}}-post-validator.py`:
```python
def rossum_hook_request_handler(payload):
    return {}
```

`rules/rdc-it-{{RUN}}-totals.json` — `payload.labels` is the label edge; the
wire shape is flat and was verified against the API:
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-totals",
  "queues": ["rdc://queues/rdc-it-{{RUN}}-invoices"],
  "description": "",
  "enabled": true,
  "trigger_condition": "True",
  "actions": [
    {
      "id": "rdc-it-{{RUN}}-a1",
      "enabled": true,
      "type": "add_label",
      "event": "validation",
      "payload": { "labels": ["rdc://labels/rdc-it-{{RUN}}-priority"] }
    }
  ]
}
```

`saved-views/rdc-it-{{RUN}}-view.json` — `query.$and` must be present and
non-empty (both were rejected in testing):
```json
{
  "id": 0,
  "url": "",
  "name": "rdc-it-{{RUN}}-view",
  "shared": true,
  "query": { "$and": [{ "status": { "$in": ["to_review"] } }] },
  "queues_filter": ["rdc://queues/rdc-it-{{RUN}}-invoices"]
}
```

- [ ] **Step 2: Write the copier**

Create `tests/live/support/snapshot.rs`:

```rust
//! Copy `testdata/live/snapshot/` into a project's env tree, substituting the
//! run id and the org url.
//!
//! Unlike every other live fixture, this one is not seeded through the API and
//! pulled — it is written straight to disk with no lockfile entries, so `rdc
//! sync` classifies every object as a LocalCreate and POSTs the whole graph.
//! That is the only way to exercise the create path in a SINGLE org: the
//! migrate-driven scenarios need `RDC_LIVE_TGT_*` and skip without it.
//!
//! It is also the only test anywhere that proves a hand-written snapshot — what
//! a user commits to git and deploys into a fresh env — works against the real
//! API.

use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use std::path::{Path, PathBuf};

/// `testdata/live/snapshot`, resolved from the crate root.
pub fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/live/snapshot")
}

/// Replace the two fixture placeholders. `{{RUN}}` is the bare run id (so a
/// fixture path reads `rdc-it-{{RUN}}-engine` and the marker stays legible);
/// `{{ORG_URL}}` is the env's organization url, required on a workspace or
/// label create and unavoidably env-specific.
pub fn substitute(raw: &str, run: &str, org_url: &str) -> String {
    raw.replace("{{RUN}}", run).replace("{{ORG_URL}}", org_url)
}

/// Copy the whole fixture tree into `envs/<env>/`, substituting placeholders in
/// both file CONTENTS and PATH components.
#[allow(dead_code)]
pub fn write_snapshot(project: &ProjectFixture, env: &str, run_id: &RunId, org_url: &str) {
    let src = snapshot_dir();
    let dst = project.path().join(format!("envs/{env}"));
    copy_dir(&src, &dst, run_id.as_str(), org_url);
}

fn copy_dir(src: &Path, dst: &Path, run: &str, org_url: &str) {
    for entry in std::fs::read_dir(src)
        .unwrap_or_else(|e| panic!("reading fixture dir {}: {e}", src.display()))
    {
        let entry = entry.expect("fixture dir entry");
        let name = entry.file_name().to_string_lossy().to_string();
        let target = dst.join(substitute(&name, run, org_url));
        if entry.path().is_dir() {
            std::fs::create_dir_all(&target)
                .unwrap_or_else(|e| panic!("creating {}: {e}", target.display()));
            copy_dir(&entry.path(), &target, run, org_url);
        } else {
            let raw = std::fs::read_to_string(entry.path())
                .unwrap_or_else(|e| panic!("reading {}: {e}", entry.path().display()));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .unwrap_or_else(|e| panic!("creating {}: {e}", parent.display()));
            }
            std::fs::write(&target, substitute(&raw, run, org_url))
                .unwrap_or_else(|e| panic!("writing {}: {e}", target.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_both_placeholders() {
        let out = substitute(
            r#"{"name":"rdc-it-{{RUN}}-ws","organization":"{{ORG_URL}}"}"#,
            "abc123",
            "https://api.example/v1/organizations/9",
        );
        assert_eq!(
            out,
            r#"{"name":"rdc-it-abc123-ws","organization":"https://api.example/v1/organizations/9"}"#
        );
    }

    /// The fixture must stay parseable and fully substituted — a stray
    /// placeholder would reach the API verbatim and fail with something far
    /// less legible than this assertion.
    #[test]
    fn every_fixture_file_parses_after_substitution() {
        let mut seen = 0;
        walk(&snapshot_dir(), &mut |path: &Path| {
            let raw = std::fs::read_to_string(path).unwrap();
            let out = substitute(&raw, "abc123", "https://api.example/v1/organizations/9");
            assert!(
                !out.contains("{{"),
                "unsubstituted placeholder left in {}",
                path.display()
            );
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                serde_json::from_str::<serde_json::Value>(&out)
                    .unwrap_or_else(|e| panic!("{} is not valid json after substitution: {e}", path.display()));
            }
            seen += 1;
        });
        assert_eq!(seen, 13, "fixture file count changed — update this test deliberately");
    }

    /// The queue MUST bind the engine. Without that binding the server never
    /// validates the schema's extracted fields against the engine's, and the
    /// ordering scenario silently stops testing the edge it exists for.
    #[test]
    fn the_queue_binds_the_engine() {
        let raw = std::fs::read_to_string(
            snapshot_dir()
                .join("workspaces/rdc-it-{{RUN}}-ws/queues/rdc-it-{{RUN}}-invoices/queue.json"),
        )
        .expect("queue fixture");
        assert!(
            raw.contains(r#""engine": "rdc://engines/rdc-it-{{RUN}}-engine""#),
            "the fixture queue must bind the fixture engine: {raw}"
        );
    }

    /// The engine field's `name` must equal the schema datapoint's `id`, or
    /// `POST /queues` refuses the create with "extracted field '<x>' is not
    /// present among names of engine fields".
    #[test]
    fn the_engine_field_name_matches_the_schema_datapoint_id() {
        let field: serde_json::Value = serde_json::from_str(&substitute(
            &std::fs::read_to_string(snapshot_dir().join(
                "engines/rdc-it-{{RUN}}-engine/fields/rdc-it-{{RUN}}-probe-field.json",
            ))
            .expect("field fixture"),
            "abc123",
            "https://api.example/v1/organizations/9",
        ))
        .unwrap();
        let schema: serde_json::Value = serde_json::from_str(&substitute(
            &std::fs::read_to_string(snapshot_dir().join(
                "workspaces/rdc-it-{{RUN}}-ws/queues/rdc-it-{{RUN}}-invoices/schema.json",
            ))
            .expect("schema fixture"),
            "abc123",
            "https://api.example/v1/organizations/9",
        ))
        .unwrap();
        let dp_id = &schema["content"][0]["children"][0]["id"];
        assert_eq!(&field["name"], dp_id, "engine field name must equal the datapoint id");
    }

    fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            if e.path().is_dir() {
                walk(&e.path(), f);
            } else {
                f(&e.path());
            }
        }
    }
}
```

Add to `tests/live/support/mod.rs`:

```rust
pub mod snapshot;
```

- [ ] **Step 3: Run the hermetic tests**

Run: `cargo test --test live snapshot:: -- --nocapture`
Expected: 4 passed. If `every_fixture_file_parses_after_substitution` reports a
count mismatch, you added or removed a fixture file — update the expected count
in the same commit, deliberately.

- [ ] **Step 4: Commit**

```bash
git add testdata/live/snapshot tests/live/support/snapshot.rs tests/live/support/mod.rs
git commit -m "test(live): a hand-authored create snapshot, engine binding included"
```

---

### Task 6: The create-ordering scenario

**Files:**
- Create: `tests/live/scenarios/ordering.rs`
- Modify: `tests/live/scenarios/mod.rs` (add `pub mod ordering;` after `organization`)

**Interfaces:**
- Consumes: `Trace` + `assert_before` (Task 1), `run_rdc_traced` (Task 2),
  engine teardown (Task 3), `write_snapshot` (Task 5).
- Produces: nothing later tasks depend on beyond the file itself, which Task 7
  extends.

**On the one edge NOT asserted.** The spec's table grouped
`engines → engine_fields → schemas → queues` as a chain. Only part of that is a
real dependency: schemas do not reference engines and engine fields do not
reference schemas, so `engine_fields → schemas` is incidental to the dispatch
order and must NOT be pinned — doing so would freeze an arbitrary choice and
fail a legitimate refactor. The load-bearing edge is `engine_fields → queues`.

- [ ] **Step 1: Write the scenario**

Create `tests/live/scenarios/ordering.rs`:

```rust
use crate::support::assert_local::load_lockfile;
use crate::support::assert_remote::assert_remote_ref_resolved;
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::converge::{assert_converged, combined};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::snapshot::write_snapshot;
use crate::support::teardown::Teardown;

/// Dependency-ordered CREATE of a whole object graph, against a real org, from
/// a hand-authored snapshot with no lockfile entries.
///
/// Two independent oracles run at once, and they cover different things.
///
/// **The server.** The fixture queue binds the fixture engine, and `POST
/// /queues` validates the queue schema's extracted fields against the bound
/// engine's field NAMES:
///
/// ```text
/// 400 non_field_errors: Engine (id: N) restriction: extracted field
///     'rdc-it-<run>-probe_field' is not present among names of engine fields
/// ```
///
/// So if `engines::push` / `engine_fields::push` ever slide back below
/// `queues::push` — where they were until `0d60d3e`, and where a promote into a
/// fresh env died on its first queue — this scenario fails with that message
/// and nothing else needs to notice.
///
/// **The trace.** `RDC_TRACE_HTTP` records every attempt, so the order is also
/// asserted directly. That matters most for the two edges the server does NOT
/// enforce: `labels → rules` (a rule action's `payload.labels` is resolved at
/// rule-create time, so a late label gives "Invalid hyperlink — No URL match"
/// only for a project that uses label actions) and the deferred relink PATCH
/// landing after both hook POSTs (if it silently stopped firing, the create
/// would still succeed and `run_after` would just be empty).
///
/// # The engine this scenario strands
///
/// Binding is what buys the server-side oracle, and it costs one engine plus
/// one field per run: a bound engine is refused deletion for up to 24 hours
/// after its queue is deleted, with no unbind escape hatch. They carry the run
/// marker, and the janitor collects them on a later run. Do NOT "fix" this by
/// dropping the binding — that would leave only the trace oracle, which mostly
/// restates what the code does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_push_create_ordering() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    write_snapshot(&project, "test", &run_id, &client.org_url);

    // One sync: pushes the whole graph, then pulls the org back.
    let (out, tr) = project.run_rdc_traced(&["sync", "test"]);
    assert!(
        out.status.success(),
        "the fresh-graph sync failed — if stderr carries \"is not present among names of \
         engine fields\", the push order regressed and engine fields are going out after \
         queues again:\n{}",
        combined(&out)
    );

    let prefix = run_id.list_prefix();

    // --- ordering, straight off the wire ---
    tr.assert_before(
        ("POST", "engines"),
        ("POST", "engine_fields"),
        "an engine field's create body carries its engine's URL",
    );
    tr.assert_before(
        ("POST", "engine_fields"),
        ("POST", "queues"),
        "POST /queues validates the schema's extracted fields against the bound engine's \
         field names, so the fields must already exist",
    );
    tr.assert_before(
        ("POST", "workspaces"),
        ("POST", "queues"),
        "the queue create body carries a resolved workspace URL",
    );
    tr.assert_before(
        ("POST", "schemas"),
        ("POST", "queues"),
        "the queue create body carries a resolved schema URL",
    );
    tr.assert_before(
        ("POST", "queues"),
        ("POST", "inboxes"),
        "an inbox belongs to a queue",
    );
    tr.assert_before(
        ("POST", "queues"),
        ("POST", "email_templates"),
        "an email template belongs to a queue",
    );
    tr.assert_before(
        ("POST", "queues"),
        ("POST", "saved_views"),
        "a saved view's queues_filter references a queue",
    );
    tr.assert_before(
        ("POST", "labels"),
        ("POST", "rules"),
        "a rule action's payload.labels is resolved against the lockfile at rule-create \
         time — a late label gives 'Invalid hyperlink - No URL match'",
    );
    tr.assert_before(
        ("POST", "hooks"),
        ("PATCH", "hooks"),
        "run_after is deferred out of the create body and PATCHed by the relink pass once \
         both hooks exist",
    );

    // --- remote truth: the refs actually resolved ---
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let q_slug = format!("{prefix}invoices");
    let queue_id = lf
        .objects
        .get("queues")
        .and_then(|m| m.get(&q_slug))
        .unwrap_or_else(|| panic!("the created queue must be in the lockfile as '{q_slug}'"))
        .id;

    assert_remote_ref_resolved(&client, "queue", queue_id, "schema")
        .await
        .expect("the queue's schema ref must resolve remotely");
    assert_remote_ref_resolved(&client, "queue", queue_id, "engine")
        .await
        .expect("the queue's engine ref must resolve remotely — the binding is the whole point");

    let validator_id = lf
        .objects
        .get("hooks")
        .and_then(|m| m.get(&format!("{prefix}validator")))
        .expect("validator hook in the lockfile")
        .id;
    let post_id = lf
        .objects
        .get("hooks")
        .and_then(|m| m.get(&format!("{prefix}post-validator")))
        .expect("post-validator hook in the lockfile")
        .id;
    let post = client.get_value("hook", post_id).await.expect("GET the post-validator");
    let run_after = post["run_after"].as_array().cloned().unwrap_or_default();
    assert!(
        run_after.iter().any(|u| u.as_str().is_some_and(|s| s.ends_with(&format!("/{validator_id}")))),
        "the deferred relink must have set run_after to the validator's URL; got {run_after:?}"
    );

    let label_id = lf
        .objects
        .get("labels")
        .and_then(|m| m.get(&format!("{prefix}priority")))
        .expect("label in the lockfile")
        .id;
    let rule_id = lf
        .objects
        .get("rules")
        .and_then(|m| m.get(&format!("{prefix}totals")))
        .expect("rule in the lockfile")
        .id;
    let rule = client.get_value("rule", rule_id).await.expect("GET the rule");
    let labels = rule["actions"][0]["payload"]["labels"].as_array().cloned().unwrap_or_default();
    assert!(
        labels.iter().any(|u| u.as_str().is_some_and(|s| s.ends_with(&format!("/{label_id}")))),
        "the rule action's label ref must have resolved to the created label; got {labels:?}"
    );

    // A hand-written snapshot must deploy in ONE cycle — no second pass to
    // settle back-refs the server filled in behind the creates.
    assert_converged(&project, "test", &prefix, "after creating the whole graph in one sync");

    drop(teardown);
}
```

- [ ] **Step 2: Register the module**

In `tests/live/scenarios/mod.rs`, add after `pub mod organization;`:

```rust
pub mod ordering;
```

- [ ] **Step 3: Compile**

Run: `cargo test --test live --no-run 2>&1 | tail -20`
Expected: compiles clean.

- [ ] **Step 4: Run it live**

Run: `cargo test --test live live_push_create_ordering -- --ignored --nocapture --test-threads=1 2>&1 | tail -60`
Expected: PASS.

The likely first-run failures and what each means:
- *"is not present among names of engine fields"* — the fixture's engine field
  `name` and schema datapoint `id` have drifted apart. The hermetic test in Task
  5 guards this; re-run `cargo test --test live snapshot::`.
- A create rejected for a missing required field — compare the fixture body
  against what a real pull writes for that kind (Task 4 Step 5 shows how).
- `assert_converged` reporting a rewritten file — rdc normalised something the
  fixture wrote differently. Align the fixture with the pulled bytes; do not
  relax the assertion.

- [ ] **Step 5: Commit**

```bash
git add tests/live/scenarios/ordering.rs tests/live/scenarios/mod.rs
git commit -m "test(live): assert the order rdc creates a fresh object graph in"
```

---

### Task 7: The delete half, and the harness docs

**Files:**
- Modify: `tests/live/scenarios/ordering.rs`
- Modify: `tests/live.rs` (module docs)

**Interfaces:**
- Consumes: everything from Tasks 1-6.
- Produces: nothing.

- [ ] **Step 1: Replace the closing lines of the scenario**

In `tests/live/scenarios/ordering.rs`, replace:

```rust
    drop(teardown);
}
```

with the delete phase:

```rust
    // -------------------------------------------------------------------------
    // Deletes: the cascade order, and skip-and-continue against a REAL refusal.
    // -------------------------------------------------------------------------
    //
    // Tombstone THIS RUN'S objects — and only this run's.
    //
    // Every path below is prefix-scoped, and that is not tidiness: the `sync`
    // above pulled the WHOLE sandbox org into this tree (a couple of hundred
    // objects, including real workspaces, hooks, rules and the org's four real
    // engines). Removing `envs/test/hooks` wholesale would tombstone all of
    // them, and the `--allow-deletes` below would then delete real content off
    // a shared org. Never widen these paths.
    //
    // The engine is deliberately left IN the tombstone set even though the
    // server will refuse it: that refusal is the point of the second assertion
    // below.
    for dir in [
        format!("envs/test/workspaces/{prefix}ws"),
        format!("envs/test/engines/{prefix}engine"),
    ] {
        std::fs::remove_dir_all(project.path().join(&dir))
            .unwrap_or_else(|e| panic!("removing {dir}: {e}"));
    }
    for file in [
        format!("envs/test/hooks/{prefix}validator.json"),
        format!("envs/test/hooks/{prefix}validator.py"),
        format!("envs/test/hooks/{prefix}post-validator.json"),
        format!("envs/test/hooks/{prefix}post-validator.py"),
        format!("envs/test/rules/{prefix}totals.json"),
        format!("envs/test/labels/{prefix}priority.json"),
        format!("envs/test/saved-views/{prefix}view.json"),
    ] {
        std::fs::remove_file(project.path().join(&file))
            .unwrap_or_else(|e| panic!("removing {file}: {e}"));
    }

    // Belt and braces: nothing outside this run may have been tombstoned. A
    // widened path above would show up here as a lockfile entry with no file,
    // BEFORE `--allow-deletes` turns it into a DELETE.
    // The check is a substring test: `TreeSnapshot::capture` keeps files whose
    // PATH contains the slug, so a lockfile entry with no matching file means
    // its file is gone — i.e. it has become a tombstone.
    //
    // Only kinds whose slug appears VERBATIM in their on-disk path can be
    // checked this way. Two are skipped because their slugs are compound and
    // the path interleaves extra segments, so the substring test would report
    // every one of them as missing:
    //
    //   email_templates  slug `<ws>/<queue>/<tpl>`
    //                    path `workspaces/<ws>/queues/<queue>/email-templates/<tpl>.json`
    //   engine_fields    slug `<engine>/<field>`
    //                    path `engines/<engine>/fields/<field>.json`
    //
    // Skipping them costs nothing: both live UNDER a parent this loop does
    // check (a workspace, an engine), so the realistic widening — removing a
    // whole top-level directory — is still caught via the parent.
    // `organization`, `mdh_*` and `workflow_*` are skipped for the same reason.
    let lf_before_del = load_lockfile(project.path(), "test").expect("lockfile before deletes");
    for (kind, entries) in &lf_before_del.objects {
        if matches!(kind.as_str(), "email_templates" | "engine_fields" | "organization")
            || kind.starts_with("mdh")
            || kind.starts_with("workflow")
        {
            continue;
        }
        for slug in entries.keys() {
            if slug.starts_with("rdc-it-") {
                continue;
            }
            let tracked =
                crate::support::converge::TreeSnapshot::capture(project.path(), "test", slug);
            assert!(
                !tracked.is_empty(),
                "about to delete something this run does not own: {kind}/{slug} has a lockfile entry but no file on disk — a tombstone path was widened"
            );
        }
    }

    let (del, dtr) = project.run_rdc_traced(&["sync", "test", "--allow-deletes"]);
    assert!(del.status.success(), "the delete pass failed: {}", combined(&del));

    // Children before parents. `saved_views` is NOT asserted: nothing
    // references a saved view, so `push::deletes` documents its position among
    // the leaves as free, and pinning it would freeze an arbitrary choice.
    for child in ["rules", "hooks", "email_templates", "inboxes"] {
        dtr.assert_before(
            ("DELETE", child),
            ("DELETE", "queues"),
            "a queue's children must be deleted before the queue",
        );
    }
    dtr.assert_before(
        ("DELETE", "queues"),
        ("DELETE", "schemas"),
        "a schema cannot be deleted while a queue references it (409 conflict_referenced)",
    );
    dtr.assert_before(
        ("DELETE", "schemas"),
        ("DELETE", "workspaces"),
        "children before parents",
    );

    // Skip-and-continue, against a refusal that is REAL and TEMPORARY.
    //
    // `run_deletes` is documented as never propagating a per-object DELETE
    // failure: it warns, tallies `DeleteCounts::failed`, LEAVES THE LOCKFILE
    // ENTRY so a later sync retries, and keeps going so every sibling and
    // parent still gets deleted. The suite's only other coverage of that
    // contract is a unique-typed email template, which is refused PERMANENTLY;
    // a bound engine is refused only until its queue finishes purging, which is
    // the case that actually needs the lockfile entry kept.
    //
    // This is not a defect pin. rdc's cascade puts engines before queues, which
    // looks wrong, but no ordering could help: the server's rule is "after the
    // queue is deleted, up to 24 hours", and `DELETE /queues` only returns `202
    // deletion_requested`.
    let engine_slug = format!("{prefix}engine");
    let stderr = combined(&del);
    assert!(
        stderr.contains(&engine_slug) && stderr.contains("delete failed (skipped)"),
        "the refused engine delete must be warned about by slug, not swallowed:\n{stderr}"
    );

    let lf_after = load_lockfile(project.path(), "test").expect("lockfile after deletes");
    assert!(
        lf_after.objects.get("engines").is_some_and(|m| m.contains_key(&engine_slug)),
        "a refused delete must KEEP its lockfile entry so a later sync retries it"
    );

    // Everything that could go, went.
    assert!(
        client.find_listed_value("queue", queue_id).await.expect("list queues").is_none()
            || client
                .get_value("queue", queue_id)
                .await
                .map(|v| v["status"] == "deletion_requested")
                .unwrap_or(false),
        "the queue must be deleted or draining"
    );
    for (kind, id) in [("hook", validator_id), ("hook", post_id), ("rule", rule_id), ("label", label_id)] {
        assert!(
            client.find_listed_value(kind, id).await.expect("list").is_none(),
            "{kind} {id} must be gone after the delete pass"
        );
    }

    drop(teardown);
}
```

The import line at the top of the file already reads
`use crate::support::assert_remote::assert_remote_ref_resolved;` and needs no
change — Task 6 never imported `assert_remote_field`.

- [ ] **Step 2: Document the two new mechanisms in the harness header**

In `tests/live.rs`, insert these two sections after the `# Reading a failure`
section and before the closing paragraph about hermetic unit tests:

```rust
//! # Asserting request ORDER
//!
//! `live_push_create_ordering` runs `rdc` with `RDC_TRACE_HTTP` set and reads
//! the CSV back through `support::trace`, so it can assert that (say) every
//! `POST /engine_fields` precedes the first `POST /queues`. Use
//! `ProjectFixture::run_rdc_traced` rather than `run_rdc` when a scenario cares
//! about order. Comparisons are last-of-A against first-of-B, which is exact
//! because `push_classified` awaits its per-kind drivers sequentially.
//!
//! # The engine that gets left behind
//!
//! `live_push_create_ordering` binds a queue to the engine it creates, on
//! purpose: that binding is what makes the SERVER enforce rdc's push order, by
//! refusing `POST /queues` when the engine lacks a field covering an extracted
//! schema field. The price is that the engine and its field cannot be deleted
//! until the queue finishes purging — up to 24 hours, with no unbind escape
//! hatch — so each run of that scenario strands one of each. They carry the run
//! marker and `live_janitor_sweep` collects them later, which is why the
//! janitor asserts every other kind is empty but only REPORTS leftover engines.
//! `live_engines_round_trip` covers the same kinds without binding anything and
//! therefore cleans up completely.
```

- [ ] **Step 3: Compile and run both new scenarios live, in order**

Run:
```sh
cargo test --test live live_engines_round_trip live_push_create_ordering \
  -- --ignored --nocapture --test-threads=1 2>&1 | tail -60
```
Expected: 2 passed.

- [ ] **Step 4: Run the whole live suite to prove nothing regressed**

Run: `cargo test --test live -- --ignored --test-threads=1 2>&1 | tail -30`
Expected: all scenarios pass (those needing `RDC_LIVE_TGT_*` report as skipped
if no second org is configured).

- [ ] **Step 5: Run the hermetic suite**

Run: `cargo test 2>&1 | tail -20`
Expected: green. Do NOT run `cargo fmt` (see Global Constraints).

- [ ] **Step 6: Commit**

```bash
git add tests/live/scenarios/ordering.rs tests/live.rs
git commit -m "test(live): pin the delete cascade and skip-and-continue on a real refusal"
```

---

## Self-review notes

Checked against the spec, 2026-08-31:

- **Spec §1 trace module** → Task 1. **§2 run_rdc_traced** → Task 2. **§3
  fixture** → Task 5. **§4 ordering scenario** → Tasks 6 (create) + 7 (delete).
  **§5 engines scenario** → Task 4. **§6 support plumbing** → Task 3.
- **One deliberate deviation from the spec.** The spec's ordering table grouped
  `engines → engine_fields → schemas → queues` as a chain; Task 6 does not
  assert `engine_fields → schemas`, because there is no dependency between them
  and pinning it would freeze an incidental dispatch choice. Noted inline in
  Task 6.
- **Type consistency:** `Trace`/`TraceLine` (Task 1) are used unchanged in Tasks
  2, 6, 7. `write_snapshot(project, env, run_id, org_url)` (Task 5) is called
  with exactly that arity in Task 6. `ProjectFixture::write_json` is introduced
  in Task 4 Step 2 and reused in Task 4 only. `LiveClient::org_url` is a public
  field (used by `support/seeder.rs` today) and is read in Task 6.
- **Unverified at plan time, to confirm on first run:** whether the
  inbox/email-template/saved-view fixture bodies need any field rdc's codec
  writes but the create schema does not require. Task 6 Step 4 lists the
  diagnosis. The hook `events` value is NOT a guess — `["annotation_content"]`
  is what `testdata/live/bodies/hooks/post-validator.json` already creates
  successfully against this sandbox.
