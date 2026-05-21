# Traditional Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `src/progress/grid.rs` and `src/progress/log.rs` with a single timestamped event-log renderer (`src/log.rs`) used by every `rdc` command, and migrate every existing log/progress callsite to the new API.

**Architecture:** One `Log` struct + closed `Action` enum, `Arc<Log>` threaded through commands. Line shape `HH:MM:SS  <action:6>  <body>`. Spinners dropped, multi-line plan blocks emitted via `Log::block`. TTY and non-TTY output are byte-identical (apart from ANSI). Migration runs in three phases: (A) ship the new module, (B) make it the single trait implementation so format changes immediately, (C) rip out the trait and migrate callsites to the direct API.

**Tech Stack:** Rust 2021, `std` only (no `chrono`), existing `cli::resolve::ColorMode` for ANSI routing. `indicatif` dep is removed at the end.

**Spec:** `docs/superpowers/specs/2026-05-21-traditional-log-design.md`

---

## File Structure

**Create:**
- `src/log.rs` — `Log`, `Action`, `now_hhmmss`, color routing, unit tests.

**Delete (after migration):**
- `src/progress/mod.rs`
- `src/progress/log.rs`
- `src/progress/grid.rs`

**Modify (signatures + callsites):**
- `src/lib.rs` — declare `log` module, drop `progress` module.
- `src/main.rs` — error-path log line.
- `src/upgrade.rs` — replace `ProgressLog`/`Spinner` calls.
- `src/api/retry.rs` — replace `ProgressHandle` with `Arc<Log>`.
- `src/cli/mod.rs`, `src/cli/auth.rs`, `src/cli/diff.rs`, `src/cli/env_picker.rs`, `src/cli/init.rs`, `src/cli/repair.rs`, `src/cli/resolve.rs`.
- `src/cli/deploy/{apply,create,realign,run,hook_secrets}.rs`.
- `src/cli/pull/{engines,engine_fields,email_templates,hooks,labels,mdh,organization,queues,rules,workflow_steps,workspaces}.rs`.
- `src/cli/push/{engines,engine_fields,email_templates,hooks,inboxes,labels,mdh,organization,queues,rules,workflow_steps,workspaces}.rs`.
- `src/cli/sync/{mod,execute,watch}.rs`.
- `Cargo.toml` — remove `indicatif`.
- `tests/api.rs`, `tests/cli_deploy.rs`, `tests/cli_diff.rs`, `tests/cli_sync.rs` and any other test fixtures pinned to old output.

---

## Task 1: Action enum with padded display

**Files:**
- Create: `src/log.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing tests**

Create `src/log.rs` with:

```rust
//! Single-renderer event log used by every `rdc` command.
//!
//! Spec: docs/superpowers/specs/2026-05-21-traditional-log-design.md

/// Closed vocabulary of action verbs. Each variant renders as a fixed-
/// width lowercase token in the action column of every log line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Action {
    // Run lifecycle
    Sync, Deploy, Pull, Push, Diff, Auth, Init, Repair, Upgr,
    // Phase
    Plan, List, Watch,
    // Per-resource
    Post, Patch, Delete, Skip,
    // Disposition
    Done, Fail, Warn, Retry, Info,
    // Watch
    Tick, Idle,
}

impl Action {
    /// Lowercase token, left-padded with spaces to exactly 6 characters.
    /// Used as the action column in every event line; the body column
    /// starts at byte offset 7 (6 + 1 space separator).
    pub fn pad(self) -> &'static str {
        match self {
            Action::Sync   => "sync  ",
            Action::Deploy => "deploy",
            Action::Pull   => "pull  ",
            Action::Push   => "push  ",
            Action::Diff   => "diff  ",
            Action::Auth   => "auth  ",
            Action::Init   => "init  ",
            Action::Repair => "repair",
            Action::Upgr   => "upgr  ",
            Action::Plan   => "plan  ",
            Action::List   => "list  ",
            Action::Watch  => "watch ",
            Action::Post   => "post  ",
            Action::Patch  => "patch ",
            Action::Delete => "delete",
            Action::Skip   => "skip  ",
            Action::Done   => "done  ",
            Action::Fail   => "fail  ",
            Action::Warn   => "warn  ",
            Action::Retry  => "retry ",
            Action::Info   => "info  ",
            Action::Tick   => "tick  ",
            Action::Idle   => "idle  ",
        }
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;

    #[test]
    fn pad_is_always_six_chars() {
        let variants = [
            Action::Sync, Action::Deploy, Action::Pull, Action::Push,
            Action::Diff, Action::Auth, Action::Init, Action::Repair, Action::Upgr,
            Action::Plan, Action::List, Action::Watch,
            Action::Post, Action::Patch, Action::Delete, Action::Skip,
            Action::Done, Action::Fail, Action::Warn, Action::Retry, Action::Info,
            Action::Tick, Action::Idle,
        ];
        for v in variants {
            assert_eq!(v.pad().len(), 6, "{v:?} pad is not 6 chars: {:?}", v.pad());
        }
    }

    #[test]
    fn pad_is_lowercase_ascii_with_trailing_spaces() {
        for v in [Action::Sync, Action::Push, Action::Delete, Action::Upgr] {
            let s = v.pad();
            let trimmed = s.trim_end();
            assert!(trimmed.chars().all(|c| c.is_ascii_lowercase()),
                "{v:?} not lowercase ascii: {s:?}");
            // Padding is on the right (left-aligned).
            assert!(!s.starts_with(' '), "{v:?} has leading space: {s:?}");
        }
    }
}
```

In `src/lib.rs`, add `pub mod log;` after the existing module declarations. Do NOT remove `pub mod progress;` yet.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib action_tests`
Expected: compile error (Action / pad not defined) — wait, they ARE defined in Step 1. Actually with the code above, both definitions and tests are in the same Step 1. So Step 2 verifies they pass.

Re-frame: write the test first (without implementation), verify red, then add implementation. Since they're in the same file, do this by writing the test block first, running `cargo test`, observing the failure (`Action` not found), then adding the enum + `pad()`.

For practicality, write both in one shot and run `cargo test --lib action_tests` once.

Expected after full Step 1: PASS for both tests.

- [ ] **Step 3: Commit**

```bash
git add src/log.rs src/lib.rs
git commit -m "feat(log): Action enum with 6-char padded display"
```

---

## Task 2: now_hhmmss helper in src/log.rs

**Files:**
- Modify: `src/log.rs`

- [ ] **Step 1: Add failing test in src/log.rs**

Append to `src/log.rs`:

```rust
/// UTC HH:MM:SS — small standalone formatter so we don't add a `chrono`
/// dep. Mirrors the helper that previously lived in `cli::sync::watch`.
pub(crate) fn now_hhmmss() -> String {
    format_hhmmss(std::time::SystemTime::now())
}

fn format_hhmmss(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_today = secs % 86400;
    let h = secs_today / 3600;
    let m = (secs_today % 3600) / 60;
    let s = secs_today % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod time_tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn format_hhmmss_at_epoch_is_midnight_utc() {
        assert_eq!(format_hhmmss(UNIX_EPOCH), "00:00:00");
    }

    #[test]
    fn format_hhmmss_wraps_modulo_day() {
        // Epoch + 1 day + 1h2m3s should print 01:02:03.
        let t = UNIX_EPOCH + Duration::from_secs(86400 + 3723);
        assert_eq!(format_hhmmss(t), "01:02:03");
    }

    #[test]
    fn format_hhmmss_pads_single_digits() {
        let t = UNIX_EPOCH + Duration::from_secs(5);
        assert_eq!(format_hhmmss(t), "00:00:05");
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --lib time_tests`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/log.rs
git commit -m "feat(log): now_hhmmss helper"
```

---

## Task 3: Log struct with event() method

**Files:**
- Modify: `src/log.rs`

- [ ] **Step 1: Add Log + event + tests**

Append to `src/log.rs`:

```rust
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::cli::resolve::ColorMode;

/// Single-renderer event log. Threaded as `Arc<Log>` through every
/// command. Lines are emitted to stderr.
pub struct Log {
    color: ColorMode,
    out: Mutex<Box<dyn Write + Send>>,
    /// Test-only clock override. Production code reads
    /// `std::time::SystemTime::now()`.
    #[cfg(test)]
    fixed_time: Option<std::time::SystemTime>,
}

impl Log {
    /// Construct a Log that writes to stderr.
    pub fn new(color: ColorMode) -> Arc<Self> {
        Arc::new(Self {
            color,
            out: Mutex::new(Box::new(std::io::stderr())),
            #[cfg(test)]
            fixed_time: None,
        })
    }

    /// Construct a Log that writes into the given sink. Test-only.
    #[cfg(test)]
    fn for_test(color: ColorMode, sink: Box<dyn Write + Send>) -> Arc<Self> {
        Arc::new(Self {
            color,
            out: Mutex::new(sink),
            fixed_time: None,
        })
    }

    #[cfg(test)]
    fn for_test_with_time(
        color: ColorMode,
        sink: Box<dyn Write + Send>,
        time: std::time::SystemTime,
    ) -> Arc<Self> {
        Arc::new(Self {
            color,
            out: Mutex::new(sink),
            fixed_time: Some(time),
        })
    }

    fn now_string(&self) -> String {
        #[cfg(test)]
        if let Some(t) = self.fixed_time {
            return format_hhmmss(t);
        }
        now_hhmmss()
    }

    /// Emit one timestamped event line:
    /// `HH:MM:SS  <action>  <body>\n`.
    pub fn event(&self, action: Action, body: &str) {
        let time = self.now_string();
        let line = format!("{time} {} {body}\n", action.pad());
        let mut out = self.out.lock().unwrap();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, UNIX_EPOCH};

    /// In-memory sink shared by tests.
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn event_line_shape() {
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14), // 12:01:14
        );
        log.event(Action::Push, "rule/finance-totals PATCH 412ms");
        assert_eq!(buf.text(), "12:01:14 push   rule/finance-totals PATCH 412ms\n");
    }

    #[test]
    fn body_column_aligned_across_actions() {
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        for a in [Action::Sync, Action::Push, Action::Patch, Action::Delete, Action::Upgr] {
            log.event(a, "x");
        }
        let text = buf.text();
        for line in text.lines() {
            // "HH:MM:SS " is 9 chars, action col is 6 chars, separator 1 char = 16 → body starts at byte 16.
            assert_eq!(&line[16..17], "x", "body misaligned in {line:?}");
        }
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --lib log_tests`
Expected: PASS (both tests).

- [ ] **Step 3: Commit**

```bash
git add src/log.rs
git commit -m "feat(log): Log struct + event() emits timestamped line"
```

---

## Task 4: Log::block for untimestamped multi-line content

**Files:**
- Modify: `src/log.rs`

- [ ] **Step 1: Add block + tests**

Inside `impl Log` append:

```rust
    /// Emit a multi-line block verbatim, without timestamp or action
    /// column. Used for plan / dry-run bodies, JSON dumps, and inline
    /// prompt UI. A trailing newline is added if missing.
    pub fn block(&self, body: &str) {
        let mut out = self.out.lock().unwrap();
        let _ = out.write_all(body.as_bytes());
        if !body.ends_with('\n') {
            let _ = out.write_all(b"\n");
        }
        let _ = out.flush();
    }

    /// Run an inline interactive prompt (auth refresh, conflict resolver,
    /// destructive delete gate). Currently a passthrough — kept as a
    /// method so callsites express intent and we can add flushing /
    /// suspending later without rewriting them.
    pub fn with_prompt<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        // Flush any pending output so the prompt doesn't tear above
        // partially-written log lines on hostile terminals.
        let _ = self.out.lock().unwrap().flush();
        f()
    }
```

In the `log_tests` module append:

```rust
    #[test]
    fn block_emits_verbatim_then_newline() {
        let buf = Buf::default();
        let log = Log::for_test(ColorMode::Plain, Box::new(buf.clone()));
        log.block("Plan: test -> prod\n  + create:  hooks (3)");
        assert_eq!(buf.text(), "Plan: test -> prod\n  + create:  hooks (3)\n");
    }

    #[test]
    fn block_does_not_double_newline() {
        let buf = Buf::default();
        let log = Log::for_test(ColorMode::Plain, Box::new(buf.clone()));
        log.block("one\ntwo\n");
        assert_eq!(buf.text(), "one\ntwo\n");
    }

    #[test]
    fn with_prompt_runs_closure_and_returns_value() {
        let buf = Buf::default();
        let log = Log::for_test(ColorMode::Plain, Box::new(buf.clone()));
        let got = log.with_prompt(|| 42);
        assert_eq!(got, 42);
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test --lib log_tests`
Expected: PASS (all five tests in `log_tests`).

- [ ] **Step 3: Commit**

```bash
git add src/log.rs
git commit -m "feat(log): block() and with_prompt() on Log"
```

---

## Task 5: Color routing for the action column

**Files:**
- Modify: `src/log.rs`

- [ ] **Step 1: Add action-category colorizer + wire into event()**

Inside `src/log.rs`, add a helper:

```rust
use crate::cli::resolve::{colorize_dim, colorize_success, colorize_warning, colorize_error, colorize_header};

/// Color category for an action. Drives which `colorize_*` helper paints
/// the action column.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ActionColor { Accent, Success, Warn, Error, Dim }

impl Action {
    fn color(self) -> ActionColor {
        match self {
            Action::Sync | Action::Deploy | Action::Pull | Action::Push
            | Action::Diff | Action::Auth | Action::Init | Action::Repair
            | Action::Upgr | Action::Plan | Action::List | Action::Watch
            | Action::Post | Action::Patch | Action::Delete           => ActionColor::Accent,
            Action::Done                                                 => ActionColor::Success,
            Action::Warn | Action::Retry                                 => ActionColor::Warn,
            Action::Fail                                                 => ActionColor::Error,
            Action::Skip | Action::Info | Action::Tick | Action::Idle    => ActionColor::Dim,
        }
    }
}
```

`cli::resolve` may not already export every helper used above — adjust to the names that exist. If `colorize_dim` is missing, add it in `src/cli/resolve.rs` as a sibling of the existing helpers (no-op under `ColorMode::Plain`, ANSI dim (`\x1b[2m...\x1b[0m`) under `ColorMode::Ansi`).

Update `Log::event` to colorize:

```rust
    pub fn event(&self, action: Action, body: &str) {
        let time = self.now_string();
        let raw_pad = action.pad();
        let action_col = match action.color() {
            ActionColor::Accent  => colorize_header(raw_pad, self.color),
            ActionColor::Success => colorize_success(raw_pad, self.color),
            ActionColor::Warn    => colorize_warning(raw_pad, self.color),
            ActionColor::Error   => colorize_error(raw_pad, self.color),
            ActionColor::Dim     => colorize_dim(raw_pad, self.color),
        };
        let time_col = colorize_dim(&time, self.color);
        let line = format!("{time_col} {action_col} {body}\n");
        let mut out = self.out.lock().unwrap();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
```

- [ ] **Step 2: Add tests**

Append to `log_tests`:

```rust
    #[test]
    fn plain_color_produces_no_ansi_escapes() {
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        for a in [Action::Sync, Action::Push, Action::Done, Action::Warn, Action::Fail, Action::Skip] {
            log.event(a, "x");
        }
        let text = buf.text();
        assert!(!text.contains('\x1b'), "ANSI escape leaked under Plain: {text:?}");
        // And the shape is still right.
        for line in text.lines() {
            assert_eq!(&line[16..17], "x", "body misaligned: {line:?}");
        }
    }
```

- [ ] **Step 3: Run tests**

Run: `cargo test --lib log_tests`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/log.rs src/cli/resolve.rs
git commit -m "feat(log): per-category color routing on the action column"
```

---

## Task 6: Bridge — implement SyncRenderer for Log, swap dispatcher, delete grid

**Goal:** at the end of this task, every command still compiles (callsites untouched) but their output goes through `Log` and is in the new format. The grid is gone.

**Files:**
- Modify: `src/log.rs` (add `SyncRenderer for Log` impl)
- Modify: `src/progress/mod.rs` (change `make_sync_renderer` to construct `Log`)
- Delete: `src/progress/grid.rs`
- Delete: `src/progress/log.rs` (the OLD one, not the new `src/log.rs`)
- Modify: `src/lib.rs` (no change to module list yet — `progress` stays as the trait host)

- [ ] **Step 1: Implement SyncRenderer for Log**

Append to `src/log.rs`:

```rust
use crate::progress::{ResourceOp, ResourceOutcome, Severity, SyncRenderer};
use crate::cli::sync::classify::ClassifiedItem;

impl SyncRenderer for Log {
    /// Old API: `phase("pushing engines")`. We don't print phase
    /// headers anymore — the action column already names the phase on
    /// every line. Lifecycle is conveyed via separate `event` calls.
    fn phase(&self, _label: &str) {
        // intentional no-op
    }

    /// Old API: `warn_line(<arbitrary>)`. Parse known prefixes
    /// (`[ok]`, `[fail]`, `!`) and route to the right action; otherwise
    /// fall through to `Info`.
    fn warn_line(&self, msg: &str) {
        let (action, body) = classify_legacy_line(msg);
        self.event(action, body);
    }

    fn resource_started(&self, _kind: &str, _slug: &str, _op: ResourceOp) {
        // No-op. The driver also emits a `warn_line("[ok] ...")` on
        // completion, which `warn_line` translates to a `post|patch|...`
        // event. The grid renderer's start/finish lifecycle has no
        // equivalent in the timestamped log.
    }

    fn resource_finished(&self, _kind: &str, _slug: &str, _outcome: ResourceOutcome) {
        // No-op (see resource_started).
    }

    fn ingest_classification(&self, _items: &[ClassifiedItem]) {
        // No-op (grid-only concern).
    }

    fn banner(&self, severity: Severity, msg: &str) {
        let action = match severity {
            Severity::Info  => Action::Info,
            Severity::Warn  => Action::Warn,
            Severity::Error => Action::Fail,
        };
        self.event(action, msg);
    }

    fn with_prompt(&self, f: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        // Mirrors Log::with_prompt but on the trait's FnMut surface.
        let _ = self.out.lock().unwrap().flush();
        f()
    }

    fn finish_ok(&self, summary: &str) {
        self.event(Action::Done, summary);
    }

    fn finish_err(&self, msg: &str) {
        self.event(Action::Fail, msg);
    }
}

/// Best-effort: map a legacy `warn_line` string to an action + body.
/// This bridge is short-lived; once all callsites use direct `Log::event`
/// the function is deleted along with the `SyncRenderer` trait.
fn classify_legacy_line(msg: &str) -> (Action, &str) {
    if let Some(rest) = msg.strip_prefix("[ok] ") {
        // [ok] hooks/x PATCH → patch hooks/x
        if let Some((head, tail)) = rest.split_once(' ') {
            if let Some(act) = action_from_token(tail.split_whitespace().next().unwrap_or("")) {
                // Keep the original body so we don't lose detail.
                return (act, msg);
            }
            let _ = head;
        }
        return (Action::Done, msg);
    }
    if let Some(_) = msg.strip_prefix("[fail] ") {
        return (Action::Fail, msg);
    }
    if let Some(_) = msg.strip_prefix("! ") {
        return (Action::Warn, msg);
    }
    (Action::Info, msg)
}

fn action_from_token(t: &str) -> Option<Action> {
    match t {
        "POST"   => Some(Action::Post),
        "PATCH"  => Some(Action::Patch),
        "DELETE" => Some(Action::Delete),
        "GET"    => Some(Action::List),
        _ => None,
    }
}
```

- [ ] **Step 2: Bridge tests**

In `log_tests`:

```rust
    #[test]
    fn warn_line_ok_patch_maps_to_patch_action() {
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        SyncRenderer::warn_line(&*log, "[ok] hooks/validator-invoices PATCH");
        let text = buf.text();
        assert!(text.starts_with("12:01:14 patch  "), "got: {text:?}");
        assert!(text.contains("[ok] hooks/validator-invoices PATCH"));
    }

    #[test]
    fn warn_line_bang_maps_to_warn_action() {
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        SyncRenderer::warn_line(&*log, "! hooks/x drift detected");
        assert!(buf.text().starts_with("12:01:14 warn   "));
    }

    #[test]
    fn finish_ok_emits_done_event() {
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        SyncRenderer::finish_ok(&*log, "Synced envs/test (2 pushed)");
        assert_eq!(buf.text(), "12:01:14 done   Synced envs/test (2 pushed)\n");
    }
```

- [ ] **Step 3: Swap the dispatcher**

Replace the body of `make_sync_renderer` in `src/progress/mod.rs`:

```rust
pub fn make_sync_renderer(
    _title: &str,
    _env: &str,
    _is_watch: bool,
) -> Arc<dyn SyncRenderer> {
    let color = crate::cli::resolve::detect_color_mode(false);
    crate::log::Log::new(color)
}
```

Remove `pub mod grid;` from `src/progress/mod.rs`. Keep `pub mod log;` for one more commit — we'll delete it in Step 5.

Add `use crate::log::Log;` if needed.

- [ ] **Step 4: Delete grid and old log renderer**

```bash
git rm src/progress/grid.rs src/progress/log.rs
```

Update `src/progress/mod.rs`:
- Remove `pub mod log;`
- Remove `pub use log::{Phase, ProgressHandle, ProgressLog, Spinner};`
- Replace `ProgressHandle` users: define a compatibility alias here:
  ```rust
  pub type ProgressHandle = Option<std::sync::Arc<dyn SyncRenderer>>;
  ```
  (was `Arc<dyn SyncRenderer>` — same shape, kept so `src/api/retry.rs` still compiles.)
- Keep the `SyncRenderer` trait and the dispatcher.
- Delete the existing dispatcher tests in `mod.rs` that reference grid (`make_sync_renderer_returns_a_trait_object`, `no_color_routes_to_log_renderer`) — they assert grid/log routing that no longer exists. Replace with one minimal test:
  ```rust
  #[cfg(test)]
  mod dispatcher_tests {
      use super::*;
      #[test]
      fn dispatcher_returns_a_log_backed_renderer() {
          let r = make_sync_renderer("t", "e", false);
          r.phase("listing remote");
          r.banner(Severity::Info, "ready");
          r.finish_ok("done");
      }
  }
  ```

- [ ] **Step 5: Fix callsites that used types from old `progress::log`**

Find offenders:

```bash
grep -rn --include='*.rs' -E 'ProgressLog|progress::log::|Spinner|Phase\b' src/ tests/
```

For each hit:
- `ProgressLog::start(...)` callsites — replace with `crate::progress::make_sync_renderer("...", env, false)` or, where the result is used as a plain handle, `crate::log::Log::new(crate::cli::resolve::detect_color_mode(false))` cast to `Arc<dyn SyncRenderer>`.
- `Spinner` / `Phase` usages — these come from `cli::auth.rs`, `cli::diff.rs`, `cli::upgrade.rs`, etc. Replace inline with `progress.warn_line(format!("[ok] {label} {summary}"))` patterns OR (preferred) emit a direct `progress.banner(Severity::Info, ...)` for the start and a `finish_ok`-style summary at the end. Since this is the bridge step, prefer the minimal mechanical translation — full per-command cleanup happens in later tasks.
- Specifically in `cli::auth.rs`: replace `let sp = progress.phase(...).item("validating token")` followed by `sp.finish_ok(o.name.clone())` with `progress.warn_line("[ok] validating token")` then `progress.banner(Severity::Info, &format!("[ok] validated {}", o.name))`.

This step is mechanical churn. Each file's diff should be obvious. Do not write tests for these intermediate translations — Task 7 onwards replaces them with the direct `Log` API and adds proper output assertions.

- [ ] **Step 6: cargo check + tests**

```bash
cargo check
cargo test --lib log_tests
cargo test --lib dispatcher_tests
```

Expected: clean compile and PASS. Integration tests in `tests/` will likely fail on output strings — that's Task 7.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(log): bridge — Log is the single SyncRenderer impl, grid deleted"
```

---

## Task 7: Update integration test fixtures for the new format

**Goal:** the integration tests in `tests/` were written against `[ok] kind summary` lines and similar shapes. Update them to match the new timestamped format.

**Files:**
- Modify: `tests/api.rs`, `tests/cli_auth.rs`, `tests/cli_deploy.rs`, `tests/cli_diff.rs`, `tests/cli_init.rs`, `tests/cli_misc.rs`, `tests/cli_repair.rs`, `tests/cli_sync.rs`, `tests/cli_version.rs` (where they assert on stderr/stdout).

- [ ] **Step 1: Find every output-string assertion**

Run:

```bash
grep -rn --include='*.rs' -E 'predicates::str|contains\(.+\[ok\]|contains\(.+\[fail\]|contains\(.+→ |contains\(.+✓|stderr|stdout' tests/
```

Inspect the hits and list (in a scratch buffer) every assertion that pins old wording.

- [ ] **Step 2: Migrate assertions, one file at a time**

For each file:
- Replace `[ok] X` checks with substring checks that don't depend on the marker:
  - Old: `predicates::str::contains("[ok] workspaces 4")`
  - New: `predicates::str::contains("list").and(predicates::str::contains("workspaces"))` — or, more robustly, just `predicates::str::contains("workspaces (4")`.
- Replace `✓ Synced envs/test` with `predicates::str::contains("done").and(predicates::str::contains("envs/test"))`.
- For timing-sensitive assertions, prefer substring contains over exact equality — timestamps differ per run.
- Where a test asserts on the FULL multi-line output (snapshot-style), regenerate by running once and pasting the new output (after a sanity scan).

- [ ] **Step 3: Run the integration suite**

```bash
cargo test --test cli_auth
cargo test --test cli_deploy
cargo test --test cli_diff
cargo test --test cli_init
cargo test --test cli_misc
cargo test --test cli_repair
cargo test --test cli_sync
cargo test --test cli_version
cargo test --test api
```

Expected: all PASS.

- [ ] **Step 4: Commit**

```bash
git add tests/
git commit -m "test: migrate fixtures to timestamped log format"
```

---

## Task 8: Migrate `src/api/retry.rs` to the direct Log API

**Files:**
- Modify: `src/api/retry.rs`

- [ ] **Step 1: Change ProgressHandle to Arc<Log>**

Open `src/api/retry.rs`. Today the function `send_with_retry` accepts a `ProgressHandle` (alias for `Option<Arc<dyn SyncRenderer>>`). Change the signature to accept `Option<&Arc<crate::log::Log>>` (or `Option<&crate::log::Log>` if no caller needs to clone — start with the reference, widen only if needed).

Inside the function, replace any `if let Some(p) = &progress { p.warn_line(&format!(...)) }` pattern with:

```rust
if let Some(log) = progress {
    log.event(crate::log::Action::Retry, &format!("{method} {path} {status} (waiting {wait_s}s)"));
}
```

- [ ] **Step 2: Update callers**

Find callers:

```bash
grep -rn --include='*.rs' 'send_with_retry\(' src/
```

For each caller, replace the `ProgressHandle` argument with `Option<&Arc<Log>>` — typically the caller has a `progress: Arc<dyn SyncRenderer>` today; you'll need to swap that to `Arc<Log>` in the caller's signature too. Since this cascades through every command, this task is the first signature flip — accept that.

Practical sequence:
1. Add a new function `send_with_retry_log` that takes `Option<&Arc<Log>>`. Keep the old `send_with_retry(ProgressHandle, ...)` as a thin shim that downcasts via `Arc::clone` and a custom helper — actually, the trait erases the type, so downcasting from `Arc<dyn SyncRenderer>` to `Arc<Log>` requires `Any`. Don't go down that path.
2. Instead, do it as a bigger atomic change: in this task, flip every command's signature from `Arc<dyn SyncRenderer>` to `Arc<Log>` (mechanical) along with the retry change. Run `cargo build` after the full sweep.

Concrete file list to edit in this single atomic change:
- `src/api/retry.rs` — signature flip + use Action::Retry
- `src/cli/auth.rs`, `src/cli/diff.rs`, `src/cli/init.rs`, `src/cli/repair.rs`, `src/cli/sync/{mod,execute,watch}.rs`, `src/cli/pull/*.rs`, `src/cli/push/*.rs`, `src/cli/deploy/{apply,create,realign,run,hook_secrets}.rs`, `src/upgrade.rs`
- `src/progress/mod.rs` — drop the trait; keep `make_sync_renderer` returning `Arc<Log>` directly; or delete `make_sync_renderer` and have each entry point call `Log::new(detect_color_mode(false))` directly.

For each: change `progress: Arc<dyn SyncRenderer>` → `log: Arc<crate::log::Log>`; leave the body's `progress.warn_line(...)` style intact for now (continues to compile because the **inherent** methods on `Log` should mirror the trait method names so the bridge isn't needed).

To keep this working, **first** add inherent methods to `Log` that mirror the trait methods (one wrapper per method, calls the corresponding event/block). Then signatures can flip and bodies keep compiling. Subsequent tasks replace `log.warn_line(...)` with idiomatic `log.event(...)` per command.

Add to `impl Log` in `src/log.rs`:

```rust
    pub fn warn_line(&self, msg: &str) { <Self as SyncRenderer>::warn_line(self, msg); }
    pub fn banner(&self, severity: Severity, msg: &str) { <Self as SyncRenderer>::banner(self, severity, msg); }
    pub fn finish_ok_compat(&self, summary: &str) { <Self as SyncRenderer>::finish_ok(self, summary); }
    pub fn finish_err_compat(&self, msg: &str) { <Self as SyncRenderer>::finish_err(self, msg); }
```

(Note: `finish_ok` / `finish_err` collide with future direct methods, hence the `_compat` suffix for the bridge era.)

- [ ] **Step 3: Build + test**

```bash
cargo check
cargo test
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor: flip command signatures from Arc<dyn SyncRenderer> to Arc<Log>"
```

---

## Task 9: Migrate `src/cli/auth.rs` to direct Log API

**Files:**
- Modify: `src/cli/auth.rs`

- [ ] **Step 1: Read the file**

```bash
sed -n '1,150p' src/cli/auth.rs
```

Identify every `log.X` call (after Task 8 the parameter is named `log`, not `progress`).

- [ ] **Step 2: Replace bridge calls with direct event/block**

| Old line | New line |
|---|---|
| `log.warn_line("[ok] validating token")` | `log.event(Action::Auth, "validating token")` |
| `sp.finish_ok(o.name.clone())` (legacy spinner) | `log.event(Action::Auth, &format!("validated org={}", o.name))` |
| `log.finish_ok_compat("Validated against ...")` | `log.event(Action::Auth, &format!("done ({org_name})"))` |
| `log.finish_err_compat("token validation failed")` | `log.event(Action::Auth, "fail token validation")` |

Use `Action::Auth` for all lifecycle, with the body indicating start/done/fail.

- [ ] **Step 3: Add a focused integration test**

In `tests/cli_auth.rs`, ensure there's at least one assertion against the new line shape — `predicates::str::contains("auth").and(predicates::str::contains("validated"))`.

- [ ] **Step 4: Build + test**

```bash
cargo test --test cli_auth
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/auth.rs tests/cli_auth.rs
git commit -m "refactor(auth): direct Log API"
```

---

## Task 10: Migrate `src/cli/diff.rs` to direct Log API

**Files:**
- Modify: `src/cli/diff.rs`

- [ ] **Step 1: Translate spinner-driven calls**

`diff.rs` uses spinner `set_message` to show live counters (`schemas (12/24)`). Drop intermediate counters — emit only:

```rust
log.event(Action::List, &format!("{kind} start"));
// ...
log.event(Action::List, &format!("{kind} ({n}, {elapsed:.1}s)"));
```

For the "differs" / "clean" verdict at the end:

```rust
log.event(Action::Diff, "clean (matches remote)");
// or
log.event(Action::Diff, &format!("done ({diffs_printed} differing)"));
```

For per-kind comparison results, use `Action::Diff` with body `<kind> differs` or `<kind> clean`.

- [ ] **Step 2: Migrate**

Run through the file and replace every `sp.finish_ok` / `progress.finish` / `progress.warn_line` call with the right `log.event(...)` + body.

- [ ] **Step 3: Update tests**

In `tests/cli_diff.rs`, update assertions to match.

- [ ] **Step 4: Build + test**

```bash
cargo test --test cli_diff
```

- [ ] **Step 5: Commit**

```bash
git add src/cli/diff.rs tests/cli_diff.rs
git commit -m "refactor(diff): direct Log API, drop spinner counters"
```

---

## Task 11: Migrate `src/cli/pull/*` drivers to direct Log API

**Files:**
- Modify: `src/cli/pull/{engines,engine_fields,email_templates,hooks,labels,mdh,organization,queues,rules,workflow_steps,workspaces}.rs`

These drivers all share the same shape: a `progress.phase("pulling X")` followed by zero or one `progress.warn_line(...)` per object and a final summary.

- [ ] **Step 1: Establish the per-driver pattern**

Each driver's top becomes:

```rust
log.event(Action::Pull, &format!("{kind} start"));
let started = std::time::Instant::now();
// ... work ...
let elapsed = started.elapsed();
log.event(Action::Pull, &format!("{kind} ({n}, {:.1}s)", elapsed.as_secs_f32()));
```

Per-object writes (rare in pull — most kinds are bulk listed) translate as:

```rust
log.event(Action::Pull, &format!("{kind}/{slug}"));
```

Skip / warning lines:

```rust
log.event(Action::Skip, &format!("{kind}/{slug} ({reason})"));
log.event(Action::Warn, &format!("{kind}/{slug} ({reason})"));
```

- [ ] **Step 2: Migrate one driver as the template**

Start with `src/cli/pull/workspaces.rs` — it's small and representative. Apply the pattern. Run `cargo test --test cli_sync` (closest integration coverage).

- [ ] **Step 3: Migrate the remaining pull drivers**

One commit per driver keeps the diff reviewable. Order: `engines`, `engine_fields`, `email_templates`, `hooks`, `labels`, `mdh`, `organization`, `queues`, `rules`, `workflow_steps`. (Workspaces done in Step 2.)

Suggested commit message per driver: `refactor(pull/<kind>): direct Log API`.

- [ ] **Step 4: Build + test after each driver**

```bash
cargo check
cargo test
```

Expected: PASS after each commit.

---

## Task 12: Migrate `src/cli/push/*` drivers to direct Log API

**Files:**
- Modify: `src/cli/push/{engines,engine_fields,email_templates,hooks,inboxes,labels,mdh,organization,queues,rules,workflow_steps,workspaces}.rs`

Push drivers emit one line per object, with `[ok]`-prefixed `warn_line` calls today. Pattern:

| Old `warn_line(...)` | New `log.event(...)` |
|---|---|
| `[ok] engines/{slug} POST (id {id})` | `Action::Post`, body `engine/{slug} id={id}` |
| `[ok] engines/{slug} PATCH` | `Action::Patch`, body `engine/{slug}` |
| `! engines/{slug} lockfile entry has no content_hash, skipping` | `Action::Skip`, body `engine/{slug} (no content_hash)` |
| `! engines/{slug} id {id} not found on remote, skipping` | `Action::Skip`, body `engine/{slug} (remote id {id} missing)` |
| `! engines/{slug} adopted remote (drift)` | `Action::Warn`, body `engine/{slug} adopted remote (drift)` |
| `! engines/{slug} remote has changed since last sync, skipping push (run `rdc sync` first)` | `Action::Skip`, body `engine/{slug} (remote changed; rdc sync first)` |
| `! engines/{slug} engines are not writable via PATCH on this Rossum org/plan (405 Method Not Allowed). Skipping all engine pushes.` | `Action::Skip`, body `engine/{slug} (PATCH 405 — engines read-only on this plan)` |

Note: the body's **noun** moves from plural-kind-prefixed (`engines/`) to singular-kind (`engine/`). Pick the convention before starting: I propose **singular** (`engine/`, `queue/`, `hook/`, `rule/`, `label/`, `inbox/`, etc.) for clarity. If you prefer plural, change every occurrence consistently.

- [ ] **Step 1: Decide noun form** — singular kind names (`engine/foo`, `hook/bar`). Document the choice at the top of `src/log.rs` as a doc comment.

- [ ] **Step 2: Migrate one driver as the template**

Start with `src/cli/push/engines.rs`. Apply the translation table above. Update `tests/cli_sync.rs` assertions that hit the engine paths.

- [ ] **Step 3: Migrate remaining push drivers**

One commit per driver: `engine_fields`, `email_templates`, `hooks`, `inboxes`, `labels`, `mdh`, `organization`, `queues`, `rules`, `workflow_steps`, `workspaces`.

- [ ] **Step 4: Build + test after each**

```bash
cargo test
```

---

## Task 13: Migrate `src/cli/deploy/*` to direct Log API

**Files:**
- Modify: `src/cli/deploy/{apply,create,realign,run,hook_secrets}.rs`

`deploy/run.rs` is the heavy one — it prints the dry-run plan body as raw `println!`. This is where `Log::block` earns its keep.

- [ ] **Step 1: deploy/run.rs — plan output**

Wrap the existing multi-line plan output:

```rust
log.event(Action::Plan, &format!("{src} -> {tgt} ({n_create} create, {n_patch} patch)"));
let mut block = String::new();
use std::fmt::Write as _;
let _ = writeln!(block, "Plan: {src} -> {tgt}");
let _ = writeln!(block, "  + create:  {}", create_parts.join(", "));
// ... existing lines, but writing into `block` instead of stdout ...
log.block(&block);
```

The big `--- create bodies (would-be POST) ---` section: same treatment.

- [ ] **Step 2: deploy/{apply,create,realign}.rs — per-object lines**

These call `progress.warn_line("[ok] hooks/x POST id=...")`. Translate using the Task 12 table.

- [ ] **Step 3: deploy/hook_secrets.rs — eprintln calls**

The `eprintln!` calls here are interactive feedback for secret rotation. Translate to:

```rust
log.event(Action::Auth, &format!("rotating secret for hook/{slug}"));
log.event(Action::Auth, &format!("secret stored for hook/{slug}"));
```

Or `Action::Info` if you'd rather keep this out of the auth bucket.

- [ ] **Step 4: Build + test**

```bash
cargo test --test cli_deploy
```

- [ ] **Step 5: Commit per file**

```bash
git add src/cli/deploy/run.rs
git commit -m "refactor(deploy/run): direct Log API + Log::block for plan body"
# repeat per file
```

---

## Task 14: Migrate `src/cli/sync/execute.rs` to direct Log API

**Files:**
- Modify: `src/cli/sync/execute.rs`

This is the densest migration — ~30 `warn_line` callsites plus conflict-prompt UI plus delete-gate prompts.

- [ ] **Step 1: Bulk-translate `warn_line` calls**

For each, identify the right action from the message content:
- Starts with `[ok]` + verb → `Action::Post` / `Patch` / `Delete` per the verb
- Starts with `!` → `Action::Skip` or `Warn` depending on whether the line implies a no-op or a problem
- Free-form info ("classifying", "conflict detected", "applying delete N/M") → `Action::Info`

- [ ] **Step 2: Wrap interactive prompts in Log::block**

Where `eprintln!` writes a multi-line "the following will be deleted" prompt body, replace with:

```rust
log.event(Action::Plan, "destructive delete review");
let mut body = String::new();
use std::fmt::Write as _;
for slug in &to_delete {
    let _ = writeln!(body, "  - {slug}");
}
log.block(&body);
// ... read confirmation ...
log.event(Action::Plan, if confirmed { "accepted" } else { "declined" });
```

Conflict resolver: same pattern.

- [ ] **Step 3: Test**

```bash
cargo test --test cli_sync
```

- [ ] **Step 4: Commit**

```bash
git add src/cli/sync/execute.rs
git commit -m "refactor(sync/execute): direct Log API, plan/prompt blocks"
```

---

## Task 15: Migrate `src/cli/sync/mod.rs` and `src/cli/sync/watch.rs`

**Files:**
- Modify: `src/cli/sync/mod.rs`, `src/cli/sync/watch.rs`

- [ ] **Step 1: sync/mod.rs — dry-run lists**

The "would pull" / "would push" / "would prompt" phases:

```rust
log.event(Action::Plan, "would pull");
let mut body = String::new();
use std::fmt::Write as _;
for it in &would_pull {
    let _ = writeln!(body, "  - {}/{}{}", it.kind, it.slug, note);
}
log.block(&body);
```

Idle state:

```rust
log.event(Action::Idle, "envs match remote");
```

- [ ] **Step 2: sync/watch.rs — drop `[HH:MM:SS]` brackets**

Today:

```rust
eprintln!("[{}] cycle failed (transient): {e:#}", now_hhmmss());
```

Becomes:

```rust
log.event(Action::Watch, &format!("cycle failed (transient): {e:#}"));
```

Auth-expired notice:

```rust
log.event(Action::Auth, "token expired — refreshing");
```

Start of watch:

```rust
log.event(Action::Watch, &format!("start envs/{env}"));
log.event(Action::Watch, &format!("polling every {}s", d.as_secs()));
```

Stop:

```rust
log.event(Action::Watch, "stopping");
```

Delete the now-unused private `now_hhmmss` from `watch.rs`. The shared one lives in `src/log.rs` (and `Log::event` uses it internally — watchers never call it directly).

- [ ] **Step 3: Test**

```bash
cargo test --test cli_sync
```

- [ ] **Step 4: Commit**

```bash
git add src/cli/sync/mod.rs src/cli/sync/watch.rs
git commit -m "refactor(sync): direct Log API in sync entry + watch, drop bracket timestamps"
```

---

## Task 16: Migrate residuals — main.rs, upgrade.rs, init.rs, repair.rs, env_picker.rs, resolve.rs, cli/mod.rs

**Files:**
- Modify: `src/main.rs`, `src/upgrade.rs`, `src/cli/init.rs`, `src/cli/repair.rs`, `src/cli/env_picker.rs`, `src/cli/resolve.rs`, `src/cli/mod.rs`

- [ ] **Step 1: main.rs**

Today: `eprintln!("error: {err:#}")`. The binary's error path doesn't have a `Log` in scope. Construct one inline:

```rust
let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
log.event(crate::log::Action::Fail, &format!("{err:#}"));
```

- [ ] **Step 2: upgrade.rs**

Replace `progress.finish(format!("Upgraded ..."))` etc. with `log.event(Action::Upgr, "done ...")`. Stand-alone `eprintln!("note: ...")` becomes `log.event(Action::Info, "rdc v… available — run `rdc upgrade`")`.

Where the function does not currently take a renderer (`upgrade.rs` has its own `progress` local), construct one inline as in main.rs Step 1.

- [ ] **Step 3: init.rs, repair.rs**

These mostly use `progress.warn_line` / `progress.finish`. Standard translation.

- [ ] **Step 4: env_picker.rs**

`eprintln!("Using the only defined env: {only}")` → either take a `&Log` param (preferred) or construct one inline. Body becomes `log.event(Action::Info, &format!("using only env: {only}"))`.

- [ ] **Step 5: resolve.rs**

`resolve.rs` exports the color helpers. Its own `eprintln!` calls (token-related warnings) translate to `log.event(Action::Auth, ...)` if a Log is in scope, otherwise stay as eprintln. **Exception:** if resolve.rs is too low in the dependency graph to import `crate::log`, keep its eprintlns — they're rare and the function is already a boundary. Note the residual eprintln explicitly with a `// log-bridge: cannot reach Log from here` comment.

- [ ] **Step 6: cli/mod.rs**

The `println!()` (empty line) call disappears entirely — blank lines are noise in a timestamped log stream.

- [ ] **Step 7: Build + test**

```bash
cargo test
```

- [ ] **Step 8: Commit**

One commit per file:

```bash
git add src/main.rs
git commit -m "refactor(main): direct Log API for top-level error path"
# repeat per file
```

---

## Task 17: Retire the SyncRenderer trait

**Files:**
- Modify: `src/progress/mod.rs` (delete or shrink to nothing)
- Modify: `src/log.rs` (remove `impl SyncRenderer for Log`, remove `warn_line` / `banner` / `finish_ok_compat` / `finish_err_compat` inherent wrappers, remove `classify_legacy_line`, remove `action_from_token`)
- Modify: `src/lib.rs` (drop `pub mod progress;`)

- [ ] **Step 1: Confirm no remaining callsites use the trait**

```bash
grep -rn --include='*.rs' -E 'SyncRenderer|warn_line|finish_ok_compat|finish_err_compat|classify_legacy_line|banner\(' src/ tests/
```

Expected: zero hits (or only inside `src/log.rs` itself, which we're about to clean).

- [ ] **Step 2: Delete the bridge**

Remove from `src/log.rs`:
- `impl SyncRenderer for Log` block
- `classify_legacy_line` and `action_from_token`
- The inherent wrappers `warn_line`, `banner`, `finish_ok_compat`, `finish_err_compat`
- The bridge-era unit tests (`warn_line_ok_patch_maps_to_patch_action`, `warn_line_bang_maps_to_warn_action`, `finish_ok_emits_done_event`)
- `use crate::progress::...` lines

Delete `src/progress/mod.rs` (the whole file) and remove `pub mod progress;` from `src/lib.rs`.

- [ ] **Step 3: Build + test**

```bash
cargo build
cargo test
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor(log): retire SyncRenderer trait and progress/ bridge"
```

---

## Task 18: Remove `indicatif` dependency

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock`

- [ ] **Step 1: Confirm zero remaining users**

```bash
grep -rn --include='*.rs' 'indicatif' src/ tests/
```

Expected: zero hits.

- [ ] **Step 2: Edit Cargo.toml**

Remove the `indicatif = "..."` line under `[dependencies]`.

- [ ] **Step 3: Rebuild**

```bash
cargo build
```

Cargo will rewrite `Cargo.lock`.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: drop unused indicatif dependency"
```

---

## Task 19: Mark superseded specs

**Files:**
- Modify: `docs/superpowers/specs/2026-05-15-progress-log-design.md`, `docs/superpowers/specs/2026-05-20-sync-grid-visualization-design.md`

- [ ] **Step 1: Add superseded banner to each**

At the top of each file (under the existing `# Title` line), add:

```markdown
> **Superseded by** `docs/superpowers/specs/2026-05-21-traditional-log-design.md` **on 2026-05-21.** Kept for historical context.
```

- [ ] **Step 2: Commit**

```bash
git add docs/superpowers/specs/
git commit -m "docs(specs): mark progress-log and sync-grid specs superseded"
```

---

## Task 20: Smoke test

**Files:** none

- [ ] **Step 1: Build release**

```bash
cargo build --release
```

- [ ] **Step 2: Run against a real env (if available locally) or a dummy one**

```bash
target/release/rdc sync test --dry-run
target/release/rdc diff test
target/release/rdc deploy test prod --dry-run
```

Spot-check that every line follows the new format. If `test`/`prod` aren't configured locally, exercise via the integration tests:

```bash
cargo test --release
```

- [ ] **Step 3: Visual review of output**

Scroll through the printed lines and confirm:
- Every line begins with `HH:MM:SS`.
- Action column is vertically aligned across lines.
- Plan / dry-run blocks appear as untimestamped blocks between timestamped lifecycle events.
- No `[ok]` / `[fail]` / `!` markers remain.
- No spinner glyphs (`▱`, `▰`, `⠙`, etc.) appear anywhere.

- [ ] **Step 4: Done**

No commit. The plan is complete.

---

## Self-review notes

**Spec coverage:**
- Format spec (HH:MM:SS, 6-char action, body) → Tasks 1, 3.
- Closed action vocabulary → Task 1.
- Color routing → Task 5.
- Spinners dropped → enforced from Task 6 onward.
- `Log::block` for multi-line plan bodies → Task 4 (definition) + Tasks 13/14/15 (usage).
- `Log::with_prompt` → Task 4.
- Removals (grid, ProgressLog, SyncRenderer trait, indicatif) → Tasks 6, 17, 18.
- Watch mode bracket removal → Task 15.
- Integration tests updated → Task 7 (bridge era) + per-driver updates in Tasks 9-16.
- Superseded specs annotated → Task 19.
- Smoke test → Task 20.

**Open ambiguity (call out for the executor):**
- Noun form (`engine/foo` vs `engines/foo`) is decided in Task 12 Step 1. If the executor wants plural, they must apply it consistently across all per-resource bodies app-wide.
