# Sync grid visualization implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the per-line event log in `rdc sync <env>` and `rdc sync --watch <env>` with an inline, kind-grouped grid of colored squares whose color is a hybrid of a freshness clock and stamps from the existing classifier. TTY-only; non-TTY auto-falls back to today's event-log format.

**Architecture:** New `SyncRenderer` trait with two implementations: `LogRenderer` (wraps the existing `ProgressLog` — current behavior) and `GridRenderer` (new, drives `indicatif::MultiProgress` as a fixed stack of bars representing header / kind-rows / footer). A dispatcher chooses based on `IsTerminal`. The two sync entry points are the only callers that change construction; per-kind push/pull drivers grow per-resource event emission consumed only by the grid renderer.

**Tech Stack:** Rust 2024 edition; `indicatif` (already a dep); `crossterm` (transitive via `inquire`, used directly for terminal size + resize events); the existing `tokio` + `anyhow` + `serde` machinery.

**Spec:** `docs/superpowers/specs/2026-05-20-sync-grid-visualization-design.md`

---

## File structure

### New files

| Path | Responsibility |
|---|---|
| `src/progress/mod.rs` | Module facade. Re-exports `ProgressLog`, `Phase`, `Spinner` from `log.rs`. Defines `SyncRenderer` trait, `ResourceOp`, `ResourceOutcome`, `Severity`, `make_sync_renderer` dispatcher. |
| `src/progress/log.rs` | The current `src/progress.rs` content, moved verbatim, plus an `impl SyncRenderer for ProgressLog` block whose grid-specific methods are no-ops. |
| `src/progress/grid.rs` | `GridRenderer`, `GridState`, `Entry`, `Banner` types. `Color` enum + `color_for` pure function. `ColorDepth` detection + ANSI emission. `MultiProgress` layout (header / kind rows / footer / banners). Implements `SyncRenderer` with the full grid behavior. |

### Modified files

| Path | Change |
|---|---|
| `src/lib.rs` | `pub mod progress;` continues to work — Rust resolves it to `src/progress/mod.rs` automatically once `src/progress.rs` is deleted. No edit needed beyond deletion. |
| `src/cli/sync/mod.rs` | `run_cycle` signature grows `renderer: Option<Arc<dyn SyncRenderer>>`. Replaces `ProgressLog::start(title)` with `renderer.unwrap_or_else(\|\| make_sync_renderer(...))`. Calls `ingest_classification` twice (pre- and post-execute). Drops the dry-run "phase" enumeration in favor of `ingest_classification`. |
| `src/cli/sync/watch.rs` | `run_watch` constructs the renderer once above the loop, passes `Some(renderer.clone())` into every `run_cycle` call. Deletes `print_cycle_summary`. |
| `src/cli/sync/execute.rs` | `resolve_conflicts` and `resolve_remote_deletes` wrap their stdin reads in `progress.with_prompt(...)`. |
| `src/cli/pull/common.rs` | `PullCtx::progress` field changes to `Arc<dyn SyncRenderer>`. `list_remote` signature: `progress: &Arc<dyn SyncRenderer>`. |
| `src/cli/pull/{hooks,rules,labels,workspaces,queues,schemas,inboxes,email_templates,engines,engine_fields,mdh,organization,workflows,workflow_steps}.rs` | Two added calls per pulled object: `progress.resource_started("<kind>", &slug, ResourceOp::Get)` before the API call, `progress.resource_finished(...)` after. Signatures change from `&Arc<ProgressLog>` to `&Arc<dyn SyncRenderer>` (mechanical). |
| `src/cli/push/{hooks,rules,labels,workspaces,queues,schemas,inboxes,email_templates,engines,engine_fields,deletes}.rs` | Same: two calls per object, signature flip. `confirm_or_refuse` in `deletes.rs` wraps its stdin read in `with_prompt`. |
| `src/cli/resolve.rs` | Prompt functions are unchanged internally; their callers (in `execute.rs`) wrap them in `with_prompt`. |
| `src/cli/sync/watch.rs` (auth refresh branch) | Wraps the existing `refresh_token_interactively(env)` call in `progress.with_prompt(...)`, emits a banner via `progress.banner(Severity::Warn, ...)`. |

### Untouched files (intentional — they keep using `ProgressLog` directly)

`src/upgrade.rs`, `src/cli/auth.rs`, `src/cli/deploy/*.rs`, `src/cli/diff.rs`. They construct `ProgressLog::start(...)` and call `phase` / `item` / `println` / `warn` / `finish` exactly as today.

---

## Task list

Twelve tasks. Each ends in a commit. Tasks 1–9 build the module without touching any caller; task 10 is the mechanical signature-flip; tasks 11–12 wire it in and add the smoke test.

### Task 1: Move `src/progress.rs` into `src/progress/log.rs`

Pure refactor. No behavior change. Existing tests must keep passing.

**Files:**
- Create: `src/progress/mod.rs`
- Create: `src/progress/log.rs`
- Delete: `src/progress.rs`

- [ ] **Step 1: Create the new directory + log file**

```bash
mkdir -p src/progress
git mv src/progress.rs src/progress/log.rs
```

- [ ] **Step 2: Create the new `src/progress/mod.rs` facade**

Content of `src/progress/mod.rs` (just re-exports for now):

```rust
//! Run-wide progress UX. Two implementations:
//!
//! * [`log::ProgressLog`] — line-based event log used by every command
//!   today. Continues to be the renderer for `deploy`, `auth`, `diff`,
//!   `upgrade`, and the non-TTY fallback for `sync`.
//! * [`grid::GridRenderer`] — kind-grouped grid of colored squares,
//!   used by `sync` and `sync --watch` on a TTY. See
//!   `docs/superpowers/specs/2026-05-20-sync-grid-visualization-design.md`.

pub mod log;
// pub mod grid; — added in Task 4.

pub use log::{Phase, ProgressLog, Spinner};
```

- [ ] **Step 3: Build + run tests**

Run: `cargo test --lib --no-fail-fast`
Expected: all green. No source file outside `src/progress/log.rs` should need changing; existing `use crate::progress::ProgressLog` paths resolve through the re-export.

- [ ] **Step 4: Commit**

```bash
git add src/progress/mod.rs src/progress/log.rs
git commit -m "refactor(progress): move src/progress.rs into a submodule"
```

---

### Task 2: Define the `SyncRenderer` trait and supporting types

No real behavior yet — just shapes. `LogRenderer` will be added in Task 3.

**Files:**
- Modify: `src/progress/mod.rs`

- [ ] **Step 1: Append the trait + types to `src/progress/mod.rs`**

```rust
use std::sync::Arc;
use crate::cli::sync::classify::ClassifiedItem;

/// Operation a per-resource event refers to. The grid renderer uses
/// these to drive the in-flight pulse and (optionally, later) to color
/// the pulse glyph by op kind. The log renderer ignores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceOp {
    Get,
    Patch,
    Post,
    Delete,
}

/// Outcome of a per-resource operation. `ConflictPending` is the
/// transient state while the resolver prompt is open.
#[derive(Debug, Clone)]
pub enum ResourceOutcome {
    Ok,
    Skipped,
    Failed(String),
    ConflictPending,
}

/// Severity of a banner. The grid renderer colors banners accordingly;
/// the log renderer routes Info to `println`, Warn/Error to `warn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
    Error,
}

/// Unified progress surface for every long-running command. Two
/// implementations: line-based ([`log::ProgressLog`]) and grid-based
/// ([`grid::GridRenderer`]). The dispatcher [`make_sync_renderer`]
/// returns the appropriate one based on stderr TTY presence.
pub trait SyncRenderer: Send + Sync {
    /// Section header / current-operation label. Both implementations
    /// honor this — the log renderer prints it; the grid renderer
    /// updates its header bar's "current op" field.
    fn phase(&self, label: &str);

    /// One free-standing line of context (warnings, retry notes, info).
    /// The log renderer routes through `MultiProgress::println`; the
    /// grid renderer queues this as a one-shot banner with the given
    /// severity inferred from the caller (Warn for most uses).
    fn warn_line(&self, msg: &str);

    /// Per-resource lifecycle: signal an API call is starting. Only the
    /// grid renderer uses this; the log renderer treats it as a no-op.
    fn resource_started(&self, kind: &str, slug: &str, op: ResourceOp);

    /// Per-resource lifecycle: signal an API call has resolved. Only
    /// the grid renderer uses this; the log renderer is a no-op.
    fn resource_finished(&self, kind: &str, slug: &str, outcome: ResourceOutcome);

    /// Fresh classification ingest — at the start and end of each
    /// cycle. The grid renderer rebuilds its entry universe (union of
    /// lockfile / local-only / remote-only); the log renderer ignores
    /// this.
    fn ingest_classification(&self, items: &[ClassifiedItem]);

    /// Queue a transient footer banner (auth expired, network 5xx, …).
    /// The grid renderer displays it in the banner slot for 5 seconds
    /// then expires; the log renderer routes through `warn`.
    fn banner(&self, severity: Severity, msg: &str);

    /// Suspend the renderer's drawing region for the duration of an
    /// inline prompt (conflict resolver, destructive delete gate, auth
    /// refresh). The grid renderer switches `MultiProgress`'s draw
    /// target to hidden; the log renderer's `mp.println` already
    /// handles spinner suspension, so the closure runs unchanged.
    fn with_prompt(&self, f: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()>;

    /// Final summary line on success. Idempotent.
    fn finish_ok(&self, summary: &str);

    /// Final summary line on error. Idempotent.
    fn finish_err(&self, msg: &str);
}

/// Dispatcher. Returns a [`GridRenderer`] when stderr is a TTY (and
/// color is available), else a [`log::ProgressLog`] wrapped in a
/// thin trait adapter. Filled in in Task 3.
pub fn make_sync_renderer(
    title: &str,
    env: &str,
    is_watch: bool,
) -> Arc<dyn SyncRenderer> {
    // Implemented in Task 3.
    let _ = (env, is_watch);
    Arc::new(log::ProgressLog::start(title).as_ref().clone())
        // ^ placeholder — Task 3 replaces this with the real dispatcher.
}
```

Note: the placeholder body in `make_sync_renderer` will not compile (`ProgressLog` is not `Clone`). That's intentional — we'll wire it in Task 3. To keep this task compilable, comment out the body for now:

```rust
pub fn make_sync_renderer(
    _title: &str,
    _env: &str,
    _is_watch: bool,
) -> Arc<dyn SyncRenderer> {
    unimplemented!("replaced in Task 3")
}
```

- [ ] **Step 2: Run `cargo check`**

Run: `cargo check --lib`
Expected: compiles. Trait is defined but no impl yet, no caller, so nothing references it.

- [ ] **Step 3: Write a sanity test for the enum variants**

Append to `src/progress/mod.rs` (cfg-test):

```rust
#[cfg(test)]
mod sync_renderer_types_tests {
    use super::*;

    #[test]
    fn resource_op_is_copy() {
        let op = ResourceOp::Patch;
        let _copy = op;
        assert_eq!(op, ResourceOp::Patch);
    }

    #[test]
    fn severity_ordering_unused() {
        // Severity is a tagged set, not ordered. Pin that we didn't
        // accidentally derive Ord.
        let _ = Severity::Warn;
        assert_ne!(Severity::Warn, Severity::Error);
    }
}
```

Run: `cargo test --lib progress::sync_renderer_types_tests`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/progress/mod.rs
git commit -m "feat(progress): define SyncRenderer trait + supporting types"
```

---

### Task 3: Implement `SyncRenderer` for `ProgressLog` and finish the dispatcher

`LogRenderer` is just `ProgressLog` with a trait impl. The dispatcher picks based on `IsTerminal` + color depth.

**Files:**
- Modify: `src/progress/log.rs`
- Modify: `src/progress/mod.rs`

- [ ] **Step 1: Add `impl SyncRenderer for ProgressLog` at the bottom of `src/progress/log.rs`**

```rust
use crate::progress::{ResourceOp, ResourceOutcome, Severity, SyncRenderer};
use crate::cli::sync::classify::ClassifiedItem;

impl SyncRenderer for ProgressLog {
    fn phase(&self, label: &str) {
        // The log renderer's existing `phase` returns a `Phase` handle
        // that's required to be alive only long enough for indicatif to
        // emit the header line. For the trait surface, we re-acquire the
        // Arc<Self> from a Weak stored at construction, call the existing
        // `phase` method, and drop the returned handle immediately.
        let arc = self.clone_arc();
        let _phase = arc.phase(label.to_string());
    }

    fn warn_line(&self, msg: &str) {
        self.warn(msg);
    }

    fn resource_started(&self, _kind: &str, _slug: &str, _op: ResourceOp) {
        // No-op for the line-based log. Per-resource events are a
        // grid-only concern.
    }

    fn resource_finished(&self, _kind: &str, _slug: &str, _outcome: ResourceOutcome) {
        // No-op for the line-based log.
    }

    fn ingest_classification(&self, _items: &[ClassifiedItem]) {
        // No-op for the line-based log. The dry-run plan enumeration
        // and the per-driver `[ok] <kind> <count>` lines already give
        // the log-mode user a full picture.
    }

    fn banner(&self, severity: Severity, msg: &str) {
        match severity {
            Severity::Info => self.println(msg),
            Severity::Warn | Severity::Error => self.warn(msg),
        }
    }

    fn with_prompt(&self, f: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        // The log renderer's `MultiProgress::println` already suspends
        // any in-flight spinner cleanly. The prompt's `eprint!` /
        // `read_line` flow doesn't need any extra coordination.
        f()
    }

    fn finish_ok(&self, summary: &str) {
        // The existing `finish` requires &Arc<Self>, but trait callers
        // hold &dyn SyncRenderer. The Arc is reconstructed from the
        // self pointer via `Arc::clone` semantics — captured at
        // construction time. See `clone_arc` helper added below.
        let arc = self.clone_arc();
        arc.finish(summary.to_string());
    }

    fn finish_err(&self, msg: &str) {
        let arc = self.clone_arc();
        arc.finish_err(msg.to_string());
    }
}
```

The `clone_arc` helper requires `ProgressLog` to hold a `Weak<Self>` of itself. Add a small constructor change — at the top of `impl ProgressLog`:

```rust
// Inside `ProgressLog::start`, after the `Arc::new(Self { ... })`:
// store a Weak<Self> in LogInner so trait methods can re-acquire the Arc.
// Modify the struct:
struct LogInner {
    // ... existing fields ...
    self_weak: std::sync::Weak<ProgressLog>,
}

impl ProgressLog {
    pub fn start(title: impl Into<String>) -> Arc<Self> {
        let title: String = title.into();
        let tty = std::io::stderr().is_terminal();
        let color = crate::cli::resolve::detect_color_mode(false);
        let mp = indicatif::MultiProgress::new();
        let arc = Arc::new_cyclic(|weak| Self {
            inner: Mutex::new(LogInner {
                title,
                mp,
                tty,
                color,
                current_phase: None,
                finished: false,
                self_weak: weak.clone(),
            }),
        });
        arc
    }

    fn clone_arc(&self) -> Arc<Self> {
        self.inner.lock().unwrap().self_weak.upgrade()
            .expect("ProgressLog dropped while trait method was running")
    }
}
```

- [ ] **Step 2: Replace the dispatcher body in `src/progress/mod.rs`**

```rust
use std::io::IsTerminal;

pub fn make_sync_renderer(
    title: &str,
    _env: &str,
    _is_watch: bool,
) -> Arc<dyn SyncRenderer> {
    if std::io::stderr().is_terminal() {
        // Task 9 swaps this for GridRenderer once the grid is built.
        // Until then, even on a TTY we fall back to ProgressLog so the
        // existing UX is unchanged for callers that already construct
        // through the dispatcher.
        log::ProgressLog::start(title)
    } else {
        log::ProgressLog::start(title)
    }
}
```

The two branches are identical for now. Task 9 differentiates them.

- [ ] **Step 3: Write an integration test that the trait surface compiles end-to-end**

Append to `src/progress/mod.rs`:

```rust
#[cfg(test)]
mod dispatcher_tests {
    use super::*;

    #[test]
    fn make_sync_renderer_returns_a_trait_object() {
        let renderer: Arc<dyn SyncRenderer> = make_sync_renderer("test", "test", false);
        renderer.phase("listing remote");
        renderer.resource_started("hooks", "validator-invoices", ResourceOp::Patch);
        renderer.resource_finished("hooks", "validator-invoices", ResourceOutcome::Ok);
        renderer.banner(Severity::Info, "ready");
        renderer.finish_ok("done");
    }
}
```

Run: `cargo test --lib progress::dispatcher_tests`
Expected: PASS. (The log renderer no-ops the grid-specific methods; only `phase` and `banner(Info)` produce output.)

- [ ] **Step 4: Run full test suite to confirm no regressions**

Run: `cargo test --lib --no-fail-fast`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/progress/log.rs src/progress/mod.rs
git commit -m "feat(progress): impl SyncRenderer for ProgressLog + dispatcher stub"
```

---

### Task 4: `Color` enum + `color_for` pure function + age-band tests

Pure module. No I/O. No dependencies on `indicatif` yet.

**Files:**
- Create: `src/progress/grid.rs`
- Modify: `src/progress/mod.rs` (uncomment `pub mod grid;`)

- [ ] **Step 1: Create `src/progress/grid.rs` with the color module**

```rust
//! Grid renderer for `rdc sync` and `rdc sync --watch`.
//!
//! Spec: docs/superpowers/specs/2026-05-20-sync-grid-visualization-design.md

use std::time::{Duration, Instant};
use crate::cli::sync::classify::SyncClass;

/// Painted color of a single square. Resolved to an ANSI escape by
/// [`emit_square`] (Task 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    FreshGreen,
    Green,
    Yellow,
    Orange,
    StaleRed,
    PendingOrange,
    EditRed,
    ConflictOutlined,
}

/// One tracked `(kind, slug)` in [`GridState`]. Filled in in Task 5.
#[derive(Debug, Clone)]
pub struct Entry {
    pub last_verified_at: Instant,
    pub class: SyncClass,
    pub in_flight: Option<crate::progress::ResourceOp>,
}

/// Map an entry to its current paint color. Stamps short-circuit the
/// freshness clock; `Clean` falls through to a 5-band age check.
///
/// The bands match section 5.1 of the spec:
///   0..=15  → FreshGreen
///   16..=60 → Green
///   61..=300 → Yellow
///   301..=900 → Orange
///   _ → StaleRed
pub fn color_for(e: &Entry, now: Instant) -> Color {
    match e.class {
        SyncClass::LocalEdit | SyncClass::LocalCreate | SyncClass::LocalDelete => {
            return Color::EditRed;
        }
        SyncClass::RemoteEdit | SyncClass::RemoteCreate | SyncClass::RemoteDelete => {
            return Color::PendingOrange;
        }
        SyncClass::BothDiverged
        | SyncClass::LocalEditRemoteDelete
        | SyncClass::LocalDeleteRemoteEdit => {
            return Color::ConflictOutlined;
        }
        SyncClass::BothDeleted => {
            // GridState evicts these at ingest time; reaching here is a bug.
            debug_assert!(false, "color_for called on BothDeleted entry");
            return Color::StaleRed;
        }
        SyncClass::Clean => {}
    }

    let age = now.saturating_duration_since(e.last_verified_at).as_secs();
    match age {
        0..=15 => Color::FreshGreen,
        16..=60 => Color::Green,
        61..=300 => Color::Yellow,
        301..=900 => Color::Orange,
        _ => Color::StaleRed,
    }
}
```

- [ ] **Step 2: Uncomment `pub mod grid;` in `src/progress/mod.rs`**

Change:

```rust
// pub mod grid; — added in Task 4.
```

to:

```rust
pub mod grid;
```

- [ ] **Step 3: Write band-edge tests**

Append to `src/progress/grid.rs`:

```rust
#[cfg(test)]
mod color_for_tests {
    use super::*;

    fn entry_clean_aged(secs: u64) -> Entry {
        Entry {
            last_verified_at: Instant::now().checked_sub(Duration::from_secs(secs)).unwrap(),
            class: SyncClass::Clean,
            in_flight: None,
        }
    }

    #[test]
    fn fresh_green_band_includes_zero_and_fifteen() {
        let now = Instant::now();
        assert_eq!(color_for(&entry_clean_aged(0), now), Color::FreshGreen);
        assert_eq!(color_for(&entry_clean_aged(15), now), Color::FreshGreen);
    }

    #[test]
    fn green_band_starts_at_sixteen_and_includes_sixty() {
        let now = Instant::now();
        assert_eq!(color_for(&entry_clean_aged(16), now), Color::Green);
        assert_eq!(color_for(&entry_clean_aged(60), now), Color::Green);
    }

    #[test]
    fn yellow_band_spans_one_to_five_minutes() {
        let now = Instant::now();
        assert_eq!(color_for(&entry_clean_aged(61), now), Color::Yellow);
        assert_eq!(color_for(&entry_clean_aged(300), now), Color::Yellow);
    }

    #[test]
    fn orange_band_spans_five_to_fifteen_minutes() {
        let now = Instant::now();
        assert_eq!(color_for(&entry_clean_aged(301), now), Color::Orange);
        assert_eq!(color_for(&entry_clean_aged(900), now), Color::Orange);
    }

    #[test]
    fn stale_red_beyond_fifteen_minutes() {
        let now = Instant::now();
        assert_eq!(color_for(&entry_clean_aged(901), now), Color::StaleRed);
        assert_eq!(color_for(&entry_clean_aged(7200), now), Color::StaleRed);
    }

    #[test]
    fn local_edit_stamp_overrides_clock_at_any_age() {
        let now = Instant::now();
        let mut e = entry_clean_aged(0);
        e.class = SyncClass::LocalEdit;
        assert_eq!(color_for(&e, now), Color::EditRed);
        let mut e = entry_clean_aged(10_000);
        e.class = SyncClass::LocalEdit;
        assert_eq!(color_for(&e, now), Color::EditRed);
    }

    #[test]
    fn remote_create_stamp_paints_pending_orange() {
        let now = Instant::now();
        let mut e = entry_clean_aged(0);
        e.class = SyncClass::RemoteCreate;
        assert_eq!(color_for(&e, now), Color::PendingOrange);
    }

    #[test]
    fn both_diverged_paints_conflict() {
        let now = Instant::now();
        let mut e = entry_clean_aged(120); // would have been Yellow
        e.class = SyncClass::BothDiverged;
        assert_eq!(color_for(&e, now), Color::ConflictOutlined);
    }
}
```

Run: `cargo test --lib progress::grid::color_for_tests`
Expected: 8 PASS.

- [ ] **Step 4: Commit**

```bash
git add src/progress/mod.rs src/progress/grid.rs
git commit -m "feat(progress/grid): Color enum and color_for pure function"
```

---

### Task 5: `GridState` ingest + eviction rules

Pure data-structure work. No rendering.

**Files:**
- Modify: `src/progress/grid.rs`

- [ ] **Step 1: Add `GridState` and ingest method**

Append to `src/progress/grid.rs`:

```rust
use std::collections::{BTreeMap, VecDeque};
use crate::cli::sync::classify::ClassifiedItem;

/// In-memory state of the grid view. Survives across watch cycles;
/// thrown away on `rdc sync` exit.
pub struct GridState {
    /// (kind, slug) → entry. Populated by `ingest_classification`.
    pub(crate) entries: BTreeMap<(String, String), Entry>,
    /// Per-kind canonical slug order (alphabetical, fixed at first
    /// observation). Drives row layout — new slugs append in
    /// alphabetical insertion order so rows don't shuffle redraws.
    pub(crate) order: BTreeMap<String, Vec<String>>,
    /// Two-cycle no-show eviction: each cycle increments this; entries
    /// remember the last cycle they were observed in. After two
    /// no-shows, evict.
    pub(crate) cycle: u64,
    /// Per-entry last-observed cycle, used by the eviction rule.
    pub(crate) last_seen_cycle: BTreeMap<(String, String), u64>,
    /// Banner queue for transient errors / auth refresh.
    pub(crate) banners: VecDeque<Banner>,
    /// Most recent `phase()` label.
    pub(crate) current_op: String,
    /// Set at construction; used by header rendering.
    pub(crate) env: String,
    pub(crate) started_at: Instant,
    pub(crate) is_watch: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Banner {
    pub severity: crate::progress::Severity,
    pub text: String,
    pub posted_at: Instant,
}

impl GridState {
    pub fn new(env: String, is_watch: bool) -> Self {
        Self {
            entries: BTreeMap::new(),
            order: BTreeMap::new(),
            cycle: 0,
            last_seen_cycle: BTreeMap::new(),
            banners: VecDeque::new(),
            current_op: String::new(),
            env,
            started_at: Instant::now(),
            is_watch,
        }
    }

    /// Fold a fresh classification snapshot into the grid state.
    /// Creates / updates / evicts entries per spec section 4.2.
    pub fn ingest(&mut self, items: &[ClassifiedItem], now: Instant) {
        self.cycle = self.cycle.wrapping_add(1);
        for it in items {
            // BothDeleted entries never get a square.
            if matches!(it.class, SyncClass::BothDeleted) {
                self.entries.remove(&(it.kind.clone(), it.slug.clone()));
                self.last_seen_cycle.remove(&(it.kind.clone(), it.slug.clone()));
                self.order.entry(it.kind.clone()).or_default()
                    .retain(|s| s != &it.slug);
                continue;
            }
            let key = (it.kind.clone(), it.slug.clone());
            self.last_seen_cycle.insert(key.clone(), self.cycle);
            let observed = !matches!(it.class, SyncClass::Clean) || it.local_hash.is_some() || it.remote_hash.is_some();
            let _ = observed; // Used below.
            let entry = self.entries.entry(key.clone()).or_insert_with(|| Entry {
                last_verified_at: now,
                class: it.class,
                in_flight: None,
            });
            entry.class = it.class;
            entry.last_verified_at = now;

            let order_for_kind = self.order.entry(it.kind.clone()).or_default();
            if !order_for_kind.iter().any(|s| s == &it.slug) {
                // Insert in alphabetical order so the row is stable.
                let pos = order_for_kind.binary_search(&it.slug).unwrap_or_else(|p| p);
                order_for_kind.insert(pos, it.slug.clone());
            }
        }

        // Eviction sweep: entries last seen ≥ 2 cycles ago are gone.
        let cutoff = self.cycle.saturating_sub(2);
        let stale: Vec<(String, String)> = self.last_seen_cycle.iter()
            .filter(|(_, &cyc)| cyc < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for key in stale {
            self.entries.remove(&key);
            self.last_seen_cycle.remove(&key);
            if let Some(order) = self.order.get_mut(&key.0) {
                order.retain(|s| s != &key.1);
            }
        }

        // Drop expired banners (≥ 5 s old).
        while let Some(front) = self.banners.front() {
            if now.saturating_duration_since(front.posted_at) > Duration::from_secs(5) {
                self.banners.pop_front();
            } else {
                break;
            }
        }
    }

    /// Update the in-flight flag for one (kind, slug). Cleared on
    /// resource_finished or on the next ingest_classification.
    pub fn mark_in_flight(&mut self, kind: &str, slug: &str, op: Option<crate::progress::ResourceOp>) {
        if let Some(e) = self.entries.get_mut(&(kind.to_string(), slug.to_string())) {
            e.in_flight = op;
        }
    }
}
```

- [ ] **Step 2: Tests**

Append:

```rust
#[cfg(test)]
mod grid_state_tests {
    use super::*;
    use crate::cli::sync::classify::ClassifiedItem;

    fn item(kind: &str, slug: &str, class: SyncClass) -> ClassifiedItem {
        ClassifiedItem {
            kind: kind.to_string(),
            slug: slug.to_string(),
            class,
            local_hash: None,
            remote_hash: None,
            base_hash: None,
        }
    }

    #[test]
    fn first_ingest_creates_entries_with_now_as_clock() {
        let mut g = GridState::new("test".into(), false);
        let now = Instant::now();
        g.ingest(&[item("labels", "audit-hold", SyncClass::Clean)], now);
        let e = &g.entries[&("labels".into(), "audit-hold".into())];
        assert_eq!(e.last_verified_at, now);
        assert!(matches!(e.class, SyncClass::Clean));
    }

    #[test]
    fn second_ingest_updates_class_and_advances_clock() {
        let mut g = GridState::new("test".into(), false);
        let t0 = Instant::now();
        g.ingest(&[item("labels", "audit-hold", SyncClass::Clean)], t0);
        let t1 = t0 + Duration::from_secs(60);
        g.ingest(&[item("labels", "audit-hold", SyncClass::LocalEdit)], t1);
        let e = &g.entries[&("labels".into(), "audit-hold".into())];
        assert_eq!(e.last_verified_at, t1);
        assert!(matches!(e.class, SyncClass::LocalEdit));
    }

    #[test]
    fn both_deleted_evicts_entry() {
        let mut g = GridState::new("test".into(), false);
        let t0 = Instant::now();
        g.ingest(&[item("labels", "gone", SyncClass::Clean)], t0);
        g.ingest(&[item("labels", "gone", SyncClass::BothDeleted)], t0);
        assert!(!g.entries.contains_key(&("labels".into(), "gone".into())));
    }

    #[test]
    fn two_cycle_no_show_evicts_entry() {
        let mut g = GridState::new("test".into(), false);
        let t0 = Instant::now();
        g.ingest(&[item("labels", "ephemeral", SyncClass::Clean)], t0);
        g.ingest(&[], t0 + Duration::from_secs(60));
        // After one no-show, entry is still present.
        assert!(g.entries.contains_key(&("labels".into(), "ephemeral".into())));
        g.ingest(&[], t0 + Duration::from_secs(120));
        // After two no-shows, entry is evicted.
        assert!(!g.entries.contains_key(&("labels".into(), "ephemeral".into())));
    }

    #[test]
    fn slug_order_is_alphabetical_and_stable() {
        let mut g = GridState::new("test".into(), false);
        let t0 = Instant::now();
        g.ingest(&[
            item("labels", "zebra", SyncClass::Clean),
            item("labels", "alpha", SyncClass::Clean),
            item("labels", "mike",  SyncClass::Clean),
        ], t0);
        assert_eq!(g.order["labels"], vec!["alpha", "mike", "zebra"]);
        // A second ingest with the same set must not reshuffle.
        g.ingest(&[
            item("labels", "mike",  SyncClass::Clean),
            item("labels", "zebra", SyncClass::Clean),
            item("labels", "alpha", SyncClass::Clean),
        ], t0);
        assert_eq!(g.order["labels"], vec!["alpha", "mike", "zebra"]);
    }

    #[test]
    fn new_slug_inserts_into_existing_order_at_alphabetical_position() {
        let mut g = GridState::new("test".into(), false);
        let t0 = Instant::now();
        g.ingest(&[
            item("labels", "alpha", SyncClass::Clean),
            item("labels", "zebra", SyncClass::Clean),
        ], t0);
        g.ingest(&[
            item("labels", "alpha", SyncClass::Clean),
            item("labels", "mike",  SyncClass::Clean),
            item("labels", "zebra", SyncClass::Clean),
        ], t0);
        assert_eq!(g.order["labels"], vec!["alpha", "mike", "zebra"]);
    }
}
```

Note: this test file imports `ClassifiedItem` and accesses its `kind`, `slug`, `class`, `local_hash`, `remote_hash`, `base_hash` fields. Verify these match the real struct shape:

```bash
grep -n "pub struct ClassifiedItem\|pub kind\|pub slug\|pub class\|pub local_hash\|pub remote_hash\|pub base_hash" src/cli/sync/classify.rs
```

If field names differ, update the `item` helper accordingly.

Run: `cargo test --lib progress::grid::grid_state_tests`
Expected: 6 PASS.

- [ ] **Step 3: Commit**

```bash
git add src/progress/grid.rs
git commit -m "feat(progress/grid): GridState ingest/eviction rules"
```

---

### Task 6: Color-depth detection + ANSI emission

Pure functions. No state, no rendering yet.

**Files:**
- Modify: `src/progress/grid.rs`

- [ ] **Step 1: Add `ColorDepth` detection**

Append:

```rust
/// Detected terminal color depth. Drives [`emit_square`] in
/// [`Self::TrueColor`] / [`Self::Color256`] / [`Self::Color16`] /
/// [`Self::None`] modes per spec section 5.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    TrueColor,
    Color256,
    Color16,
    None,
}

pub fn detect_color_depth() -> ColorDepth {
    if std::env::var("NO_COLOR").is_ok() {
        return ColorDepth::None;
    }
    match std::env::var("COLORTERM").as_deref() {
        Ok("truecolor") | Ok("24bit") => ColorDepth::TrueColor,
        _ => match std::env::var("TERM").as_deref() {
            Ok(t) if t.contains("256color") => ColorDepth::Color256,
            _ => ColorDepth::Color16,
        },
    }
}
```

- [ ] **Step 2: Add `emit_square`**

Append:

```rust
/// Build the ANSI escape sequence for one square. Output is always two
/// cells wide (the square) plus one gap cell. The caller appends this
/// directly to the row string fed to `MultiProgress::set_message`.
///
/// `dim` halves the brightness — used by the in-flight pulse.
pub fn emit_square(color: Color, depth: ColorDepth, dim: bool) -> String {
    let (r, g, b) = match color {
        Color::FreshGreen      => (0x1f, 0x6e, 0x3e),
        Color::Green           => (0x2a, 0x8a, 0x4b),
        Color::Yellow          => (0xc7, 0x9a, 0x2b),
        Color::Orange          => (0xd8, 0x61, 0x2e),
        Color::StaleRed        => (0xa5, 0x2a, 0x2a),
        Color::PendingOrange   => (0xe8, 0x96, 0x22),
        Color::EditRed         => (0xff, 0x3b, 0x30),
        Color::ConflictOutlined=> (0xc9, 0x30, 0x30),
    };
    let (r, g, b) = if dim {
        ((r as f32 * 0.7) as u8, (g as f32 * 0.7) as u8, (b as f32 * 0.7) as u8)
    } else {
        (r, g, b)
    };

    let outline = matches!(color, Color::ConflictOutlined);

    match depth {
        ColorDepth::TrueColor => {
            if outline {
                format!("\x1b[48;2;{r};{g};{b}m\x1b[38;2;255;209;102m▏▕\x1b[0m ")
            } else {
                format!("\x1b[48;2;{r};{g};{b}m  \x1b[0m ")
            }
        }
        ColorDepth::Color256 => {
            // Approximate via the 6x6x6 color cube. Index = 16 + 36r' + 6g' + b'
            // where r' = round(r/255 * 5).
            let idx = 16
                + 36 * ((r as u16 * 5 / 255) as u16)
                + 6  * ((g as u16 * 5 / 255) as u16)
                + ((b as u16 * 5 / 255) as u16);
            if outline {
                format!("\x1b[48;5;{idx}m\x1b[38;5;221m▏▕\x1b[0m ")
            } else {
                format!("\x1b[48;5;{idx}m  \x1b[0m ")
            }
        }
        ColorDepth::Color16 => {
            // Collapse to 3 buckets: green / yellow / red.
            let ansi_bg = match color {
                Color::FreshGreen | Color::Green => 42, // green bg
                Color::Yellow | Color::Orange | Color::PendingOrange => 43, // yellow bg
                Color::StaleRed | Color::EditRed | Color::ConflictOutlined => 41, // red bg
            };
            format!("\x1b[{ansi_bg}m  \x1b[0m ")
        }
        ColorDepth::None => {
            // ASCII fallback: "· " (clean band) / "o " (aging) / "x " (stamp/conflict).
            match color {
                Color::FreshGreen | Color::Green => "·  ".to_string(),
                Color::Yellow | Color::Orange | Color::PendingOrange => "o  ".to_string(),
                Color::StaleRed | Color::EditRed | Color::ConflictOutlined => "x  ".to_string(),
            }
        }
    }
}
```

- [ ] **Step 3: Tests**

Append:

```rust
#[cfg(test)]
mod emit_square_tests {
    use super::*;

    #[test]
    fn truecolor_edit_red_emits_known_escape() {
        let s = emit_square(Color::EditRed, ColorDepth::TrueColor, false);
        assert_eq!(s, "\x1b[48;2;255;59;48m  \x1b[0m ");
    }

    #[test]
    fn truecolor_conflict_emits_outline_glyphs() {
        let s = emit_square(Color::ConflictOutlined, ColorDepth::TrueColor, false);
        assert!(s.contains("▏▕"), "conflict outline missing: {s:?}");
    }

    #[test]
    fn dim_reduces_brightness() {
        let bright = emit_square(Color::FreshGreen, ColorDepth::TrueColor, false);
        let dim    = emit_square(Color::FreshGreen, ColorDepth::TrueColor, true);
        assert_ne!(bright, dim);
    }

    #[test]
    fn no_color_emits_ascii_only() {
        assert_eq!(emit_square(Color::FreshGreen, ColorDepth::None, false), "·  ");
        assert_eq!(emit_square(Color::EditRed,     ColorDepth::None, false), "x  ");
        assert_eq!(emit_square(Color::PendingOrange, ColorDepth::None, false), "o  ");
    }

    #[test]
    fn detect_color_depth_respects_no_color() {
        // Use unsafe scoped env var manipulation via a helper guard; for
        // simplicity test the value-returning bits via an explicit env
        // snapshot. SAFETY: tests run single-threaded for env modification.
        let saved = std::env::var("NO_COLOR").ok();
        // SAFETY: see above
        unsafe { std::env::set_var("NO_COLOR", "1"); }
        assert_eq!(detect_color_depth(), ColorDepth::None);
        unsafe { match saved { Some(v) => std::env::set_var("NO_COLOR", v), None => std::env::remove_var("NO_COLOR") }; }
    }
}
```

Run: `cargo test --lib progress::grid::emit_square_tests`
Expected: 5 PASS.

- [ ] **Step 4: Commit**

```bash
git add src/progress/grid.rs
git commit -m "feat(progress/grid): ColorDepth detection + ANSI emit_square"
```

---

### Task 7: `GridRenderer` skeleton + bar allocation

Constructs the `MultiProgress` stack, allocates header + kind rows + footer slots. No rendering yet — `set_message` calls write empty strings. Implements `SyncRenderer` with the new-method bodies wired to internal state mutation; `phase` / `ingest_classification` / `banner` push into the state, but no actual draw output yet.

**Files:**
- Modify: `src/progress/grid.rs`

- [ ] **Step 1: Add `GridRenderer` struct + constructor + `SyncRenderer` impl**

Append:

```rust
use std::sync::Mutex;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle, ProgressDrawTarget};
use crate::progress::{ResourceOp, ResourceOutcome, Severity, SyncRenderer};

/// Maximum kinds we pre-allocate row slots for. The classifier emits 11
/// SyncClass kinds (`workspaces`, `queues`, `schemas`, `inboxes`,
/// `email_templates`, `hooks`, `rules`, `labels`, `engines`,
/// `engine_fields`, `mdh`) plus 3 read-only (`organization`,
/// `workflows`, `workflow_steps`). 16 is conservative.
const MAX_KINDS: usize = 16;
/// Max footer entries shown before "+ N more".
const MAX_FOOTER: usize = 12;
const MAX_BANNERS: usize = 2;
/// Max continuation rows per kind. With ~500 squares in a 60-col-wide
/// budget, one kind needs ≤ 30 rows worst case; 32 is safe headroom.
const MAX_CONT_ROWS: usize = 32;

pub struct GridRenderer {
    inner: Mutex<GridInner>,
}

struct GridInner {
    state: GridState,
    color_depth: ColorDepth,
    mp: MultiProgress,
    /// Stable bar handles, allocated at construction:
    header: ProgressBar,
    /// Per-kind row groups. Up to MAX_KINDS, each with up to
    /// MAX_CONT_ROWS continuation bars (used when the row wraps).
    kind_rows: Vec<Vec<ProgressBar>>,
    separator: ProgressBar,
    banner_slots: Vec<ProgressBar>,
    footer_header: ProgressBar,
    footer_slots: Vec<ProgressBar>,
    footer_more: ProgressBar,
    /// Mapping kind → row group index. Populated lazily on first
    /// observation of a kind.
    kind_index: BTreeMap<String, usize>,
    next_kind_slot: usize,
    finished: bool,
}

impl GridRenderer {
    pub fn new(env: String, is_watch: bool) -> Arc<Self> {
        let mp = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(8));
        let style_plain = ProgressStyle::with_template("{msg}").unwrap();
        let style_spinner = ProgressStyle::with_template("{spinner} {msg}")
            .unwrap()
            .tick_strings(&["|", "/", "-", "\\"]);

        let mk_plain = |msg: &str| {
            let bar = mp.add(ProgressBar::new(1));
            bar.set_style(style_plain.clone());
            bar.set_message(msg.to_string());
            bar
        };
        let header = mp.add(ProgressBar::new(1));
        header.set_style(style_spinner.clone());
        header.enable_steady_tick(Duration::from_millis(250));
        header.set_message(String::new());

        let mut kind_rows: Vec<Vec<ProgressBar>> = Vec::with_capacity(MAX_KINDS);
        for _ in 0..MAX_KINDS {
            let mut group = Vec::with_capacity(MAX_CONT_ROWS);
            for _ in 0..MAX_CONT_ROWS {
                group.push(mk_plain(""));
            }
            kind_rows.push(group);
        }

        let separator = mk_plain("");
        let banner_slots: Vec<_> = (0..MAX_BANNERS).map(|_| mk_plain("")).collect();
        let footer_header = mk_plain("");
        let footer_slots: Vec<_> = (0..MAX_FOOTER).map(|_| mk_plain("")).collect();
        let footer_more = mk_plain("");

        let color_depth = detect_color_depth();

        Arc::new(Self {
            inner: Mutex::new(GridInner {
                state: GridState::new(env, is_watch),
                color_depth,
                mp,
                header,
                kind_rows,
                separator,
                banner_slots,
                footer_header,
                footer_slots,
                footer_more,
                kind_index: BTreeMap::new(),
                next_kind_slot: 0,
                finished: false,
            }),
        })
    }
}

impl SyncRenderer for GridRenderer {
    fn phase(&self, label: &str) {
        let mut g = self.inner.lock().unwrap();
        g.state.current_op = label.to_string();
        // Header re-render lives in Task 8.
    }

    fn warn_line(&self, msg: &str) {
        // Wrap as a Warn banner so it lands in the banner queue.
        self.banner(Severity::Warn, msg);
    }

    fn resource_started(&self, kind: &str, slug: &str, op: ResourceOp) {
        let mut g = self.inner.lock().unwrap();
        g.state.mark_in_flight(kind, slug, Some(op));
    }

    fn resource_finished(&self, kind: &str, slug: &str, _outcome: ResourceOutcome) {
        let mut g = self.inner.lock().unwrap();
        g.state.mark_in_flight(kind, slug, None);
    }

    fn ingest_classification(&self, items: &[ClassifiedItem]) {
        let mut g = self.inner.lock().unwrap();
        g.state.ingest(items, Instant::now());
        // Re-render in Task 8.
    }

    fn banner(&self, severity: Severity, msg: &str) {
        let mut g = self.inner.lock().unwrap();
        // Dedup within 10 s by exact text (spec 9.3).
        let now = Instant::now();
        if let Some(b) = g.state.banners.iter_mut().find(|b| b.text == msg
            && now.saturating_duration_since(b.posted_at) < Duration::from_secs(10))
        {
            // Append "(×N)" suffix; bump posted_at.
            let new_text = if let Some(open) = b.text.find(" (×") {
                let count: u32 = b.text[open + 4..b.text.len() - 1].parse().unwrap_or(1);
                format!("{} (×{})", &b.text[..open], count + 1)
            } else {
                format!("{msg} (×2)")
            };
            b.text = new_text;
            b.posted_at = now;
            return;
        }
        g.state.banners.push_back(Banner { severity, text: msg.to_string(), posted_at: now });
    }

    fn with_prompt(&self, f: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        let mp_clone = {
            let g = self.inner.lock().unwrap();
            g.mp.clone()
        };
        mp_clone.set_draw_target(ProgressDrawTarget::hidden());
        let result = f();
        mp_clone.set_draw_target(ProgressDrawTarget::stderr_with_hz(8));
        result
    }

    fn finish_ok(&self, summary: &str) {
        let mut g = self.inner.lock().unwrap();
        if g.finished { return; }
        g.finished = true;
        // Commit final frame: switch to hidden, then println the current
        // bar contents as permanent lines. Task 11 fills this in; for now
        // just println the summary so behavior parity holds.
        let _ = g.mp.println(format!("DONE: {summary}"));
    }

    fn finish_err(&self, msg: &str) {
        let mut g = self.inner.lock().unwrap();
        if g.finished { return; }
        g.finished = true;
        let _ = g.mp.println(format!("FAIL: {msg}"));
    }
}
```

- [ ] **Step 2: Smoke test the constructor**

Append:

```rust
#[cfg(test)]
mod grid_renderer_skeleton_tests {
    use super::*;

    #[test]
    fn new_constructs_without_panicking() {
        let r = GridRenderer::new("test".into(), false);
        // We can call the trait methods; they shouldn't panic.
        r.phase("listing remote");
        r.resource_started("hooks", "foo", ResourceOp::Get);
        r.resource_finished("hooks", "foo", ResourceOutcome::Ok);
        r.banner(Severity::Info, "ready");
        r.finish_ok("done");
    }

    #[test]
    fn banner_dedup_appends_count_suffix() {
        let r = GridRenderer::new("test".into(), false);
        r.banner(Severity::Warn, "auth expired");
        r.banner(Severity::Warn, "auth expired");
        r.banner(Severity::Warn, "auth expired");
        let g = r.inner.lock().unwrap();
        assert_eq!(g.state.banners.len(), 1);
        assert_eq!(g.state.banners[0].text, "auth expired (×3)");
    }
}
```

Run: `cargo test --lib progress::grid::grid_renderer_skeleton_tests`
Expected: 2 PASS.

- [ ] **Step 3: Commit**

```bash
git add src/progress/grid.rs
git commit -m "feat(progress/grid): GridRenderer skeleton + bar allocation"
```

---

### Task 8: Header / kind-row / footer rendering

Wire `set_message` calls per bar so `ingest_classification` and `phase` actually paint pixels.

**Files:**
- Modify: `src/progress/grid.rs`

- [ ] **Step 1: Add a private `repaint` method on `GridRenderer`**

Insert into `impl GridRenderer` block:

```rust
impl GridRenderer {
    /// Width budget for square cells (after the 18-char label prefix
    /// and one space separator). Falls back to 80-column if crossterm
    /// can't read the size.
    fn cells_per_line(&self) -> usize {
        let cols = crossterm::terminal::size().map(|(c, _)| c as usize).unwrap_or(80);
        let budget = cols.saturating_sub(18 + 1);
        (budget / 3).max(1)
    }

    fn repaint(&self) {
        let mut g = self.inner.lock().unwrap();
        let now = Instant::now();
        let cells_per_line = self.cells_per_line();

        // ---- Header ----
        let (clean, pending, conflict) = count_buckets(&g.state);
        let uptime = if g.state.is_watch {
            format!(" · uptime {}", fmt_uptime(now.saturating_duration_since(g.state.started_at)))
        } else { String::new() };
        let header_msg = format!(
            "rdc sync{watch} {env} · {clean} clean · {pending} pending · {conflict} conflict · {op}{uptime}",
            watch = if g.state.is_watch { " --watch" } else { "" },
            env = g.state.env,
            clean = clean,
            pending = pending,
            conflict = conflict,
            op = if g.state.current_op.is_empty() { "idle" } else { &g.state.current_op },
            uptime = uptime,
        );
        g.header.set_message(header_msg);

        // ---- Kind rows ----
        // Assign kind → row group slots in first-seen order (which, since
        // ingest processes items in classifier-emitted order, follows the
        // canonical kind order: workspaces, queues, schemas, ...).
        let kinds: Vec<String> = g.state.order.keys().cloned().collect();
        for kind in &kinds {
            if !g.kind_index.contains_key(kind) {
                let slot = g.next_kind_slot;
                if slot >= MAX_KINDS {
                    continue; // pathological — more kinds than we allocated
                }
                g.kind_index.insert(kind.clone(), slot);
                g.next_kind_slot += 1;
            }
        }
        for (kind, slot) in g.kind_index.clone() {
            let slugs = match g.state.order.get(&kind) { Some(v) => v.clone(), None => continue };
            let count = slugs.len();
            let label = format!("{:<16} ({:>2}) ", kind, count);
            let mut squares = String::new();
            let mut line_idx = 0usize;
            for (i, slug) in slugs.iter().enumerate() {
                let entry = match g.state.entries.get(&(kind.clone(), slug.clone())) { Some(e) => e, None => continue };
                let color = color_for(entry, now);
                let dim = entry.in_flight.is_some()
                    && (now.elapsed().subsec_millis() / 125) % 2 == 1;
                squares.push_str(&emit_square(color, g.color_depth, dim));
                if (i + 1) % cells_per_line == 0 {
                    // Commit the current line, start a new continuation row.
                    let line_msg = if line_idx == 0 {
                        format!("{}{}", label, squares)
                    } else {
                        format!("{:<19}{}", " ", squares)
                    };
                    if line_idx < MAX_CONT_ROWS {
                        g.kind_rows[slot][line_idx].set_message(line_msg);
                    }
                    line_idx += 1;
                    squares.clear();
                }
            }
            if !squares.is_empty() && line_idx < MAX_CONT_ROWS {
                let line_msg = if line_idx == 0 {
                    format!("{}{}", label, squares)
                } else {
                    format!("{:<19}{}", " ", squares)
                };
                g.kind_rows[slot][line_idx].set_message(line_msg);
                line_idx += 1;
            }
            // Clear unused continuation rows for this kind.
            for r in line_idx..MAX_CONT_ROWS {
                g.kind_rows[slot][r].set_message(String::new());
            }
        }
        // Clear rows for kinds we don't have anymore (eviction).
        for kind_slot in g.next_kind_slot..MAX_KINDS {
            for r in 0..MAX_CONT_ROWS {
                g.kind_rows[kind_slot][r].set_message(String::new());
            }
        }

        // ---- Banners ----
        for (i, slot_bar) in g.banner_slots.iter().enumerate() {
            if let Some(b) = g.state.banners.get(i) {
                let prefix = match b.severity {
                    Severity::Info  => "·",
                    Severity::Warn  => "!",
                    Severity::Error => "✖",
                };
                slot_bar.set_message(format!("{prefix} {}", b.text));
            } else {
                slot_bar.set_message(String::new());
            }
        }

        // ---- Footer ----
        let mut non_clean: Vec<(&(String, String), &Entry)> = g.state.entries.iter()
            .filter(|(_, e)| !matches!(e.class, SyncClass::Clean))
            .collect();
        // Severity descending then kind/slug ascending.
        non_clean.sort_by_key(|(k, e)| (severity_rank(e.class), k.0.clone(), k.1.clone()));

        let total = g.state.entries.len();
        let problem_count = non_clean.len();

        if problem_count == 0 {
            g.footer_header.set_message(format!("all clean ({total})"));
        } else {
            g.footer_header.set_message("current state:".to_string());
        }
        let to_show = non_clean.iter().take(MAX_FOOTER).collect::<Vec<_>>();
        for (i, slot_bar) in g.footer_slots.iter().enumerate() {
            if let Some(((kind, slug), entry)) = to_show.get(i).copied() {
                let tag = match entry.class {
                    SyncClass::BothDiverged
                    | SyncClass::LocalEditRemoteDelete
                    | SyncClass::LocalDeleteRemoteEdit => "conflict",
                    SyncClass::LocalEdit | SyncClass::LocalCreate | SyncClass::LocalDelete => "edit    ",
                    SyncClass::RemoteEdit | SyncClass::RemoteCreate | SyncClass::RemoteDelete => "pending ",
                    SyncClass::Clean | SyncClass::BothDeleted => "        ",
                };
                slot_bar.set_message(format!("  {tag}  {kind}/{slug}"));
            } else {
                slot_bar.set_message(String::new());
            }
        }
        if problem_count > MAX_FOOTER {
            g.footer_more.set_message(format!(
                "  + {} more (run `rdc sync --dry-run` for full list)",
                problem_count - MAX_FOOTER
            ));
        } else {
            g.footer_more.set_message(String::new());
        }
    }
}

fn count_buckets(state: &GridState) -> (usize, usize, usize) {
    let mut clean = 0;
    let mut pending = 0;
    let mut conflict = 0;
    for e in state.entries.values() {
        match e.class {
            SyncClass::Clean => clean += 1,
            SyncClass::LocalEdit | SyncClass::LocalCreate | SyncClass::LocalDelete
            | SyncClass::RemoteEdit | SyncClass::RemoteCreate | SyncClass::RemoteDelete => pending += 1,
            SyncClass::BothDiverged
            | SyncClass::LocalEditRemoteDelete
            | SyncClass::LocalDeleteRemoteEdit => conflict += 1,
            SyncClass::BothDeleted => {}
        }
    }
    (clean, pending, conflict)
}

fn fmt_uptime(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 { format!("{}s", secs) }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else { format!("{}h{}m", secs / 3600, (secs % 3600) / 60) }
}

fn severity_rank(c: SyncClass) -> u8 {
    match c {
        SyncClass::BothDiverged
        | SyncClass::LocalEditRemoteDelete
        | SyncClass::LocalDeleteRemoteEdit => 0,
        SyncClass::LocalEdit | SyncClass::LocalCreate | SyncClass::LocalDelete => 1,
        SyncClass::RemoteEdit | SyncClass::RemoteCreate | SyncClass::RemoteDelete => 2,
        SyncClass::Clean => 3,
        SyncClass::BothDeleted => 4,
    }
}
```

- [ ] **Step 2: Call `repaint` from `phase` / `ingest_classification` / `banner` / `resource_*`**

Update the `SyncRenderer for GridRenderer` impl: at the end of each method that mutates state, call `drop(g); self.repaint();` (release the lock first, then repaint which re-acquires).

Concretely change the impl bodies so each method:

```rust
fn phase(&self, label: &str) {
    {
        let mut g = self.inner.lock().unwrap();
        g.state.current_op = label.to_string();
    }
    self.repaint();
}

fn resource_started(&self, kind: &str, slug: &str, op: ResourceOp) {
    {
        let mut g = self.inner.lock().unwrap();
        g.state.mark_in_flight(kind, slug, Some(op));
    }
    self.repaint();
}

// ... same shape for resource_finished, ingest_classification, banner
```

- [ ] **Step 3: Snapshot-ish test of the rendered header**

Append:

```rust
#[cfg(test)]
mod repaint_tests {
    use super::*;
    use crate::cli::sync::classify::ClassifiedItem;

    fn item(kind: &str, slug: &str, class: SyncClass) -> ClassifiedItem {
        ClassifiedItem {
            kind: kind.to_string(), slug: slug.to_string(), class,
            local_hash: None, remote_hash: None, base_hash: None,
        }
    }

    #[test]
    fn header_includes_counts_and_current_op() {
        let r = GridRenderer::new("test".into(), true);
        r.phase("listing remote");
        r.ingest_classification(&[
            item("labels", "a", SyncClass::Clean),
            item("labels", "b", SyncClass::LocalEdit),
            item("hooks",  "x", SyncClass::BothDiverged),
        ]);
        // Read the header bar's message by grabbing the rendered string
        // through `bar.message()`.
        let g = r.inner.lock().unwrap();
        let msg = g.header.message();
        assert!(msg.contains("test"), "{msg}");
        assert!(msg.contains("1 clean"), "{msg}");
        assert!(msg.contains("1 pending"), "{msg}");
        assert!(msg.contains("1 conflict"), "{msg}");
        assert!(msg.contains("listing remote"), "{msg}");
    }

    #[test]
    fn footer_lists_non_clean_resources_by_severity() {
        let r = GridRenderer::new("test".into(), false);
        r.ingest_classification(&[
            item("labels", "a", SyncClass::Clean),
            item("labels", "b", SyncClass::LocalEdit),
            item("hooks",  "x", SyncClass::BothDiverged),
            item("queues", "q", SyncClass::RemoteEdit),
        ]);
        let g = r.inner.lock().unwrap();
        assert_eq!(g.footer_header.message(), "current state:");
        // Severity rank: conflict (0) < edit (1) < pending (2).
        assert!(g.footer_slots[0].message().contains("conflict"), "{:?}", g.footer_slots[0].message());
        assert!(g.footer_slots[1].message().contains("edit"), "{:?}", g.footer_slots[1].message());
        assert!(g.footer_slots[2].message().contains("pending"), "{:?}", g.footer_slots[2].message());
    }

    #[test]
    fn footer_collapses_when_all_clean_but_kind_rows_persist() {
        let r = GridRenderer::new("test".into(), false);
        r.ingest_classification(&[
            item("labels", "a", SyncClass::Clean),
            item("hooks",  "x", SyncClass::Clean),
        ]);
        let g = r.inner.lock().unwrap();
        assert!(g.footer_header.message().starts_with("all clean ("));
        // Kind rows must still have content (the squares).
        let labels_slot = *g.kind_index.get("labels").unwrap();
        assert!(!g.kind_rows[labels_slot][0].message().is_empty());
    }
}
```

Run: `cargo test --lib progress::grid::repaint_tests`
Expected: 3 PASS.

- [ ] **Step 4: Commit**

```bash
git add src/progress/grid.rs
git commit -m "feat(progress/grid): header, kind-row, and footer rendering"
```

---

### Task 9: Wire the dispatcher to construct `GridRenderer` on TTY + color

Now `make_sync_renderer` actually picks based on `IsTerminal` + color depth.

**Files:**
- Modify: `src/progress/mod.rs`

- [ ] **Step 1: Replace the dispatcher body**

```rust
use std::io::IsTerminal;

pub fn make_sync_renderer(
    title: &str,
    env: &str,
    is_watch: bool,
) -> Arc<dyn SyncRenderer> {
    let is_tty = std::io::stderr().is_terminal();
    let color = grid::detect_color_depth();
    if is_tty && color != grid::ColorDepth::None {
        grid::GridRenderer::new(env.to_string(), is_watch)
    } else {
        log::ProgressLog::start(title)
    }
}
```

- [ ] **Step 2: Sanity test**

Append to `src/progress/mod.rs`:

```rust
#[cfg(test)]
mod dispatcher_routing_tests {
    use super::*;

    #[test]
    fn no_color_routes_to_log_renderer() {
        let saved = std::env::var("NO_COLOR").ok();
        // SAFETY: tests in this file run single-threaded.
        unsafe { std::env::set_var("NO_COLOR", "1"); }
        let r = make_sync_renderer("test", "test", false);
        // We can't easily downcast through dyn SyncRenderer. Instead,
        // verify behavior: a LogRenderer's `phase` prints to stderr;
        // a GridRenderer's `phase` would not (it updates internal
        // state). We can call `phase` and check that it doesn't panic,
        // and that ingest_classification is a no-op for the log
        // renderer (LogRenderer ignores it).
        r.phase("listing remote");
        r.ingest_classification(&[]);
        r.finish_ok("done");
        unsafe { match saved { Some(v) => std::env::set_var("NO_COLOR", v), None => std::env::remove_var("NO_COLOR") }; }
    }
}
```

Run: `cargo test --lib progress::dispatcher_routing_tests`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/progress/mod.rs
git commit -m "feat(progress): wire dispatcher to choose grid vs log by tty+color"
```

---

### Task 10: Refactor caller signatures from `&Arc<ProgressLog>` to `&Arc<dyn SyncRenderer>`

Mechanical signature flip across the push/pull/sync code paths. Large in line count, small in semantic change. The trait surface preserved by `LogRenderer` means no behavior changes for non-sync callers either.

**Files:**
- Modify: `src/cli/pull/common.rs` (`PullCtx::progress`, `list_remote`).
- Modify: `src/cli/pull/{hooks,rules,labels,workspaces,queues,schemas,inboxes,email_templates,engines,engine_fields,mdh,organization,workflows,workflow_steps}.rs` (16 files).
- Modify: `src/cli/push/{hooks,rules,labels,workspaces,queues,schemas,inboxes,email_templates,engines,engine_fields,deletes,mod}.rs` (12 files).
- Modify: `src/cli/sync/execute.rs`.

- [ ] **Step 1: Change `PullCtx::progress` field type**

In `src/cli/pull/common.rs`:

```rust
// Before:
//   progress: &'a Arc<crate::progress::ProgressLog>,
// After:
pub struct PullCtx<'a> {
    // ... other fields ...
    pub progress: Arc<dyn crate::progress::SyncRenderer>,
}
```

(If `PullCtx` currently holds `progress: &Arc<ProgressLog>` it carries a lifetime. Switch to an owned `Arc<dyn SyncRenderer>` — the trait object can't be referenced through the `&` without `'a + ?Sized` bounds and it's not worth the syntactic cost.)

Verify the actual current shape first:

```bash
grep -n "pub struct PullCtx" -A 15 src/cli/pull/common.rs
```

If the field is `progress: &'a Arc<ProgressLog>`, switch to `progress: Arc<dyn crate::progress::SyncRenderer>` (owned, not referenced). Update `'a` if it becomes unused.

- [ ] **Step 2: Update `list_remote` signature**

```rust
pub async fn list_remote(
    ctx: &mut PullCtx<'_>,
    env_cfg: &crate::config::EnvConfig,
    env: &str,
    token: &str,
    progress: &Arc<dyn crate::progress::SyncRenderer>,
) -> Result<RemoteCatalog> { /* unchanged body */ }
```

Every callsite of `list_remote` that passes `&progress` (where `progress: Arc<ProgressLog>`) needs the same Arc cast — but since `Arc<ProgressLog>` impls `SyncRenderer`, the coercion is automatic at the call site as long as the expected type is `&Arc<dyn SyncRenderer>`. Verify:

```bash
grep -rn "list_remote(" src --include='*.rs'
```

- [ ] **Step 3: Sweep all per-kind drivers — must be atomic**

The signature flip is atomic: every caller that imports `progress: &Arc<ProgressLog>` must move to `progress: &Arc<dyn crate::progress::SyncRenderer>` in this task, otherwise the codebase won't compile. There is no partial state.

For each of the 28 driver files (`src/cli/pull/{14 kinds}.rs` + `src/cli/push/{11 kinds + mod.rs}.rs` + `src/cli/sync/execute.rs`):

1. Change `progress: &Arc<ProgressLog>` → `progress: &Arc<dyn crate::progress::SyncRenderer>`.
2. Locate the existing `let phase = progress.phase(...); let sp = phase.item(name); ...; sp.finish_ok(summary);` sequence. The trait's `phase(label: &str)` returns `()` — there is no `Phase` handle on the trait surface. Replace the sequence with a single `progress.phase("...")` call. The per-object accounting that used to come through `phase.item(...).finish_ok(...)` is now provided by `resource_started` / `resource_finished` (added in Task 11) — for this task we just drop the per-object spinner.

Concrete example. In `src/cli/pull/labels.rs`:

```rust
// OLD:
let phase = progress.phase("pulling labels");
let sp = phase.item("labels");
let listed = client.list_labels().await?;
for label in &listed {
    process_one(label, ...).await?;
}
sp.finish_ok(format!("{} pulled", listed.len()));

// NEW (Task 10 — signature flip + drop per-object spinner):
progress.phase("pulling labels");
let listed = client.list_labels().await?;
for label in &listed {
    process_one(label, ...).await?;
}
progress.warn_line(&format!("[ok] labels {} pulled", listed.len()));
// ^ keeps the line-format output that CI greps for; Task 11 adds
//   per-label resource_started/resource_finished calls inside the loop.
```

The `Phase` / `Spinner` types stay alive on `ProgressLog` and remain in use by callers that import them directly: `src/upgrade.rs`, `src/cli/auth.rs`, `src/cli/deploy/*.rs`, `src/cli/diff.rs`. Those callers are untouched.

Apply this pattern to all 28 files in one pass. Use the project-wide search-replace as a starting point; manually fix each call to `.phase(...).item(...)` chain.

For `src/cli/sync/execute.rs`, the prompt-wrapping logic stays as `prompt_resolve(...)` etc. for now — Task 12 adds the `with_prompt` wrapping.

- [ ] **Step 4: Compile + run the full test suite**

Run: `cargo build --lib && cargo test --lib --no-fail-fast`
Expected: green. If any caller breaks because it depended on the old `progress.phase("...")` returning a `Phase`, fix it inline by either:
- Keeping that caller on `&Arc<ProgressLog>` (it's a non-sync caller — deploy, diff, etc.); or
- Replacing the `phase.item(...)` pattern with `progress.warn_line(...)`-style calls.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(progress): flip PullCtx + per-kind driver signatures to SyncRenderer trait"
```

---

### Task 11: Per-resource event emission in push/pull drivers

Add `progress.resource_started(...)` and `progress.resource_finished(...)` around every API call in the push/pull drivers. The grid renderer animates the pulse; the log renderer no-ops.

**Files:**
- Modify: all push/pull driver files listed in Task 10's Files block.

- [ ] **Step 1: For each pull driver, wrap the per-object work**

Example for `src/cli/pull/labels.rs`:

```rust
for label in &catalog.labels {
    let slug = pick_slug(label);
    progress.resource_started("labels", &slug, ResourceOp::Get);
    let result = process_one_label(label, &slug, /* ... */).await;
    match &result {
        Ok(_)  => progress.resource_finished("labels", &slug, ResourceOutcome::Ok),
        Err(e) => progress.resource_finished("labels", &slug, ResourceOutcome::Failed(e.to_string())),
    }
    result?;
}
```

Apply the same pattern to all 14 pull drivers and 11 push drivers. The `kind` string passed to the trait methods MUST match the SyncClass kind names — `workspaces`, `queues`, `schemas`, `inboxes`, `email_templates`, `hooks`, `rules`, `labels`, `engines`, `engine_fields`, `mdh`, `organization`, `workflows`, `workflow_steps`. Verify by cross-referencing `from_catalog_scan_lockfile` in `src/cli/sync/mod.rs`.

- [ ] **Step 2: Compile**

Run: `cargo build --lib`
Expected: compiles.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test --lib --no-fail-fast`
Expected: green. Existing tests don't observe per-resource events (since the log renderer no-ops them) so nothing should change.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(sync): emit per-resource events from push/pull drivers"
```

---

### Task 12: Wire `make_sync_renderer` into `run_cycle` + `watch::run_watch`, add ingest_classification calls, behavior-parity gate

The terminal task. Replaces `ProgressLog::start(title)` in sync paths with the dispatcher, threads the renderer through, calls `ingest_classification` twice per cycle, wraps prompts in `with_prompt`, deletes `print_cycle_summary`, commits the final frame to scrollback.

**Files:**
- Modify: `src/cli/sync/mod.rs`
- Modify: `src/cli/sync/watch.rs`
- Modify: `src/cli/sync/execute.rs`
- Modify: `src/cli/push/deletes.rs`

- [ ] **Step 1: Update `run_cycle` to take an optional pre-constructed renderer**

In `src/cli/sync/mod.rs`:

```rust
pub(crate) async fn run_cycle(
    env: &str,
    interactive: bool,
    dry_run: bool,
    diff: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    renderer: Option<Arc<dyn crate::progress::SyncRenderer>>,
) -> Result<CycleOutcome> {
    // ... existing arg validation ...

    let title = if dry_run { format!("rdc sync {env} (dry run)") } else { format!("rdc sync {env}") };
    let renderer = renderer.unwrap_or_else(|| crate::progress::make_sync_renderer(&title, env, false));

    // ... existing list_remote + scan + classify ...

    renderer.ingest_classification(&classified);

    if dry_run { /* existing block, but call renderer.finish_ok instead of progress.finish */ }

    // execute::run gets `&renderer` (instead of `&progress`):
    let outcome = execute::run(&mut ctx, &catalog, &classified, no_push, no_pull, allow_deletes, interactive, &renderer).await?;

    // Re-classify post-execute and re-ingest so squares that flipped Clean go bright green.
    let classified_after = from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile, overlay.as_ref());
    renderer.ingest_classification(&classified_after);

    // ... existing lockfile.save + _index.md regen ...

    renderer.finish_ok(&format!("Synced envs/{env} ({} changed, {:.1}s)", outcome.items_pushed + outcome.items_pulled, started.elapsed().as_secs_f32()));
    Ok(outcome)
}

pub async fn run(/* unchanged args */) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let paths = Paths::for_env(&cwd, env);
    let _lock = crate::cli::sync::lock::EnvLock::acquire(&paths.env_lock(), std::time::Duration::from_secs(30))?;
    run_cycle(env, interactive, dry_run, diff, allow_deletes, no_push, no_pull, None).await?;
    Ok(())
}
```

- [ ] **Step 2: Update `watch::run_watch` to construct the renderer once**

In `src/cli/sync/watch.rs`, around the existing `eprintln!("watching envs/{env}/ ...");`:

```rust
let renderer = crate::progress::make_sync_renderer(&format!("rdc sync --watch {env}"), env, true);

// Initial reconcile uses the same renderer:
{
    let _lock = ...;
    crate::cli::sync::run_cycle(env, interactive, false, false, allow_deletes, no_push, no_pull, Some(renderer.clone())).await?;
}

// Then loop, passing Some(renderer.clone()) into each run_cycle call inside event_loop.
```

Delete `print_cycle_summary` and its only call site — the grid is the summary now. The verbose-mode `(idle)` ticks also go away; replaced by the header's `current_op` field.

- [ ] **Step 3: Wrap inline prompts in `with_prompt`**

In `src/cli/sync/execute.rs::resolve_conflicts`, around the `prompt_resolve(...)` call:

```rust
let resolution = progress.with_prompt(&mut || {
    let r = prompt_resolve(&mut input, &mut std::io::stderr(), &diff_view)?;
    // store r in an outer Option via a Cell or Mutex; with_prompt's signature uses anyhow::Result<()>.
    *out.borrow_mut() = Some(r);
    Ok(())
})?;
```

Concrete shape: declare `let out = std::cell::RefCell::new(None);` before the closure; inside the closure, populate it; after `with_prompt` returns, unwrap. The closure must be `FnMut() -> Result<()>` per the trait.

Same wrapping in `resolve_remote_deletes` and in `src/cli/push/deletes.rs::confirm_or_refuse`.

In `src/cli/sync/watch.rs`, the existing 401 branch:

```rust
Err(e) if crate::api::anyhow_has_status(&e, 401) => {
    renderer.banner(crate::progress::Severity::Warn, &format!("[{}] auth expired — refreshing token", now_hhmmss()));
    renderer.with_prompt(&mut || futures::executor::block_on(crate::cli::auth::refresh_token_interactively(env)))?;
    crate::cli::sync::run_cycle(env, interactive, false, false, allow_deletes, no_push, no_pull, Some(renderer.clone())).await?
}
```

(The `block_on` is a small awkwardness because `with_prompt` is sync; if needed, an async variant `with_prompt_async` can be added in a follow-up. For now keep it sync to match the trait.)

- [ ] **Step 4: Commit the final frame on exit**

In `GridRenderer::finish_ok` (and `finish_err`), replace the placeholder `mp.println("DONE: ...")` with the real commit sequence:

```rust
fn finish_ok(&self, summary: &str) {
    let g = self.inner.lock().unwrap();
    if g.finished { return; }
    // Walk every bar; println its current msg to commit to scrollback.
    let commit = |bar: &ProgressBar| {
        let msg = bar.message();
        if !msg.is_empty() {
            let _ = g.mp.println(msg);
        }
    };
    commit(&g.header);
    for row in &g.kind_rows { for bar in row { commit(bar); } }
    commit(&g.separator);
    for bar in &g.banner_slots { commit(bar); }
    commit(&g.footer_header);
    for bar in &g.footer_slots { commit(bar); }
    commit(&g.footer_more);
    let _ = g.mp.println(format!("DONE: {summary}"));
    g.mp.set_draw_target(ProgressDrawTarget::hidden());
    drop(g);
    // Mark finished after release to avoid relocking.
    self.inner.lock().unwrap().finished = true;
}
```

(Same shape for `finish_err`, with `FAIL: ` prefix.)

- [ ] **Step 5: Run the full test suite — behavior parity gate**

Run: `cargo test --no-fail-fast`
Expected: green. The existing watch and sync integration tests use non-TTY stderr (test harness), so they hit the `LogRenderer` path. Output must match the pre-refactor format byte-for-byte.

Inspect carefully:
- `tests/sync_test.rs` (if present) — fixtures around `[ok] sync envs/...`.
- Any snapshot tests under `tests/` or `proptest-regressions/`.

If any test fails on output format, the fix is in `LogRenderer`'s trait impl — not in the test. The contract is "byte-for-byte parity with today's output."

- [ ] **Step 6: Smoke test by hand**

Run: `cargo run -- sync test --dry-run` against a fixture project (`testdata/` or a scratch directory). Then `cargo run -- sync --watch test`. Note the visual behavior; confirm:
- Grid appears.
- Squares are green initially.
- A local edit (touch a `.json` file in the env) flips its square to red within ~1 second.
- Ctrl-C in watch mode prints the final frame plus the `stopped after ...` line.

If grid visuals are off (wrap is wrong, colors look bad in your terminal, conflict outline glyphs don't render), file follow-up tickets against this PR — they're out of scope for the merge.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(sync): wire grid renderer into sync + watch with parity gate"
```

---

## Self-review pass

After all 12 tasks are merged, run a final check:

1. **Spec coverage.** Walk each section of `docs/superpowers/specs/2026-05-20-sync-grid-visualization-design.md` and confirm a task implements it. Open issues for any gaps.
2. **Behavior parity in CI.** The behavior-parity gate in Task 12 Step 5 is the contract. If anything broke in CI logs, that's a regression — fix before merging.
3. **Visual smoke.** Open the README example env (or a fixture) in `cargo run -- sync --watch <env>` on macOS Terminal.app, iTerm2, and Linux GNOME Terminal. Note any rendering quirks; file follow-ups (per spec section 9, the conflict outline glyph is explicitly flagged for in-implementation tuning).

## Out of scope for this plan (per spec section 11)

- No `--ui` flag, no env-var override.
- No configurable thresholds; bands are hard-coded constants.
- No mouse interaction, no alt-screen, no keybindings beyond Ctrl-C.
- No persistent state across runs.
- No grid for `rdc deploy` / `rdc diff`.

If any of those come up during implementation, add to follow-up tickets — don't expand this plan.
