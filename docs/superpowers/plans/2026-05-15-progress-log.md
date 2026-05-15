# Progress Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the indicatif progress bar used by `rdc pull` / `rdc push` / `rdc deploy` / `rdc sync` with a per-kind / per-object event log that shows inline spinners during waits.

**Architecture:** Rewrite `src/progress.rs` around three types — `ProgressLog` (run handle), `Phase` (section header), `Spinner` (one in-flight line that resolves to ✓ / ⚠ / ✗). All three wrap `indicatif::ProgressBar::new_spinner()` for the TTY spinner and degrade cleanly to plain stderr lines in non-TTY mode. The old `OverallProgress` type and its bar template are deleted; all ~16 callers migrate to the new API.

**Tech Stack:** Rust (existing rdc codebase), `indicatif` (already a dep — keep, use spinners only), `anyhow`, `serde_json`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-05-15-progress-log-design.md` — read it before starting.

---

## File Structure

**Modify:**
- `src/progress.rs` — full rewrite: delete `OverallProgress` + `Inner::Bar` + `Inner::Log`, replace with `ProgressLog` / `Phase` / `Spinner`. Unit tests use a `Write` sink for capture-and-assert.
- `src/cli/pull/mod.rs` and `src/cli/pull/{common,workspaces,queues,hooks,rules,labels,engines,engine_fields,workflows,workflow_steps,email_templates,organization,mdh}.rs` — migrate every `progress.tick(...)` / `progress.start_phase(...)` call to the new API.
- `src/cli/push/mod.rs` and `src/cli/push/{workspaces,queues,schemas,inboxes,email_templates,hooks,rules,labels,engines,engine_fields,deletes,scan}.rs` — same migration.
- `src/cli/deploy/{run,apply,create}.rs` — same.
- `src/cli/sync/{mod,execute,watch}.rs` — same. (Watch loop already prints its own per-cycle lines; only the inner cycle's progress threading needs migration.)
- `tests/cli_pull.rs` (deleted earlier — N/A), `tests/cli_push.rs` (deleted earlier — N/A), `tests/cli_deploy.rs`, `tests/cli_sync.rs`, `tests/cli_repair.rs`, `tests/cli_misc.rs` — update assertions that scanned for the old `→ phase: …` / `✓ phase: N items, Xs` shape.
- `README.md` — if any example output shows the old bar, update.

**Delete:** Nothing structurally — the module file stays at `src/progress.rs`. Only the type names and their bodies change.

---

## Task 1: New API alongside the old

Build the new `ProgressLog` / `Phase` / `Spinner` types. Old `OverallProgress` stays so existing callers compile; we migrate them in Task 2 and remove the old types in Task 3.

**Files:**
- Modify: `src/progress.rs` (append new types; do NOT delete old yet).

- [ ] **Step 1: Add the new types and skeleton methods**

In `src/progress.rs`, **append** (don't replace) the following at the bottom of the file (after the existing `Drop for OverallProgress` impl):

```rust
// =====================================================================
// NEW API — see docs/superpowers/specs/2026-05-15-progress-log-design.md
// =====================================================================

use indicatif::{ProgressBar, ProgressStyle};

/// Run-wide handle for the new event-log UX. Created once at the top of a
/// pull/push/deploy/sync run; cloneable into per-driver scopes.
pub struct ProgressLog {
    inner: Mutex<LogInner>,
}

struct LogInner {
    title: String,
    /// MultiProgress so the active spinner doesn't tear the surrounding
    /// stderr writes. Lives for the duration of the run.
    mp: indicatif::MultiProgress,
    /// Whether stderr is a real TTY (drives spinner animation vs plain lines).
    tty: bool,
    /// Color mode resolved at construction; cheap to copy.
    color: crate::cli::resolve::ColorMode,
    /// The most-recently-printed phase label, used so `Phase::item` knows
    /// the right indent context. None before the first `phase()` call.
    current_phase: Option<String>,
    /// Whether `finish` / `finish_err` has been called.
    finished: bool,
}

impl ProgressLog {
    /// Print the header line and return the handle.
    /// `title` shows as the first line (`→ rdc sync test`).
    pub fn start(title: impl Into<String>) -> Arc<Self> {
        let title: String = title.into();
        let tty = std::io::stderr().is_terminal();
        let color = crate::cli::resolve::detect_color_mode(false);
        let mp = indicatif::MultiProgress::new();
        // Header line goes through `eprintln!` (not the MP) because there's
        // no in-flight item to animate — it's a stable line at the top.
        let header = format!("→ {title}");
        eprintln!("{}", crate::cli::resolve::colorize_header(&header, color));
        Arc::new(Self {
            inner: Mutex::new(LogInner {
                title,
                mp,
                tty,
                color,
                current_phase: None,
                finished: false,
            }),
        })
    }

    /// Start a labelled section. The label appears on its own line.
    /// Subsequent `item()` calls (on the returned `Phase`) are indented
    /// under it. Calling `phase()` again finalizes the previous section's
    /// blank-line padding and starts a fresh one.
    pub fn phase(self: &Arc<Self>, label: impl Into<String>) -> Phase {
        let label: String = label.into();
        {
            let mut inner = self.inner.lock().unwrap();
            // Blank line before each subsequent phase.
            if inner.current_phase.is_some() {
                eprintln!();
            }
            inner.current_phase = Some(label.clone());
            // Phase headers go directly to stderr — no spinner.
            // `eprintln!` is fine because MultiProgress has no active bar
            // at this exact moment (Phase::item is what creates spinners).
            let styled = crate::cli::resolve::colorize_header(&label, inner.color);
            eprintln!("{styled}");
        }
        Phase {
            log: self.clone(),
            label,
        }
    }

    /// Final summary line on success. Idempotent — calling twice is a no-op.
    pub fn finish(self: &Arc<Self>, summary: impl Into<String>) {
        let summary: String = summary.into();
        let mut inner = self.inner.lock().unwrap();
        if inner.finished {
            return;
        }
        inner.finished = true;
        // Blank line before the final line if any phase was emitted.
        if inner.current_phase.is_some() {
            eprintln!();
        }
        let line = format!("✔ {summary}");
        let styled = crate::cli::resolve::colorize_final_ok(&line, inner.color);
        eprintln!("{styled}");
    }

    /// Final summary line on error. Idempotent.
    pub fn finish_err(self: &Arc<Self>, msg: impl Into<String>) {
        let msg: String = msg.into();
        let mut inner = self.inner.lock().unwrap();
        if inner.finished {
            return;
        }
        inner.finished = true;
        if inner.current_phase.is_some() {
            eprintln!();
        }
        let line = format!("✗ {msg}");
        let styled = crate::cli::resolve::colorize_error(&line, inner.color);
        eprintln!("{styled}");
    }
}

/// One section of the run (`listing remote`, `scanning local`, `pushing`).
/// Items inside the section render indented; spinners attach to items.
pub struct Phase {
    log: Arc<ProgressLog>,
    label: String,
}

impl Phase {
    /// Start a single in-flight item. The line begins with a spinner and
    /// the given name; the caller resolves it via `Spinner::finish_*`.
    pub fn item(&self, name: impl Into<String>) -> Spinner {
        let name: String = name.into();
        let (mp, tty, color) = {
            let inner = self.log.inner.lock().unwrap();
            (inner.mp.clone(), inner.tty, inner.color)
        };
        // 2-space indent under the phase header.
        let bar = if tty {
            let bar = mp.add(ProgressBar::new_spinner());
            bar.set_style(
                ProgressStyle::with_template("  {spinner} {msg}")
                    .unwrap()
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(80));
            bar.set_message(name.clone());
            bar
        } else {
            // Non-TTY: no animation. Print the start line plainly; on
            // resolve we emit the final line in place of the spinner.
            // We still need a ProgressBar handle for the API, but it does
            // nothing in non-TTY mode (indicatif auto-suppresses).
            let bar = mp.add(ProgressBar::hidden());
            bar.set_message(name.clone());
            bar
        };
        Spinner {
            bar,
            name,
            color,
            tty,
            started: std::time::Instant::now(),
            resolved: false,
        }
    }

    /// One-shot summary line without a spinner. Used for steps where the
    /// work is so fast a spinner would flicker (`scanning local`,
    /// `classifying`). Renders as `  <content>`.
    pub fn line(&self, content: impl Into<String>) {
        let content: String = content.into();
        let inner = self.log.inner.lock().unwrap();
        let line = format!("  {content}");
        // No animation — direct eprint.
        eprintln!("{line}");
        let _ = inner; // suppress unused
    }
}

/// One in-flight item line. Hold while work runs; call `finish_*` to
/// resolve. If dropped without resolving, finalizes as "(cancelled)".
pub struct Spinner {
    bar: ProgressBar,
    name: String,
    color: crate::cli::resolve::ColorMode,
    tty: bool,
    started: std::time::Instant,
    resolved: bool,
}

impl Spinner {
    /// Update the in-flight message (rare; most callers don't need this).
    pub fn set_message(&self, msg: impl Into<String>) {
        self.bar.set_message(msg.into());
    }

    /// Resolve with a ✓ and optional summary. Examples:
    /// - `sp.finish_ok("4")` → `  ✓ workspaces 4`
    /// - `sp.finish_ok("PATCH 0.4s")` → `  ✓ hooks/x → PATCH 0.4s`
    /// (The caller passes the trailing summary; the name is preserved.)
    pub fn finish_ok(mut self, summary: impl Into<String>) {
        if self.resolved { return; }
        self.resolved = true;
        let summary: String = summary.into();
        let elapsed = self.started.elapsed();
        let line = format_final_line("✓", &self.name, &summary, elapsed, self.color);
        if self.tty {
            self.bar.finish_with_message(line);
        } else {
            eprintln!("  {line}");
        }
    }

    /// Resolve with a ⚠ and one-line warning.
    pub fn finish_warn(mut self, msg: impl Into<String>) {
        if self.resolved { return; }
        self.resolved = true;
        let msg: String = msg.into();
        let elapsed = self.started.elapsed();
        let line = format_final_line("⚠", &self.name, &msg, elapsed, self.color);
        if self.tty {
            self.bar.finish_with_message(line);
        } else {
            eprintln!("  {line}");
        }
    }

    /// Resolve with a ✗ and one-line error.
    pub fn finish_err(mut self, msg: impl Into<String>) {
        if self.resolved { return; }
        self.resolved = true;
        let msg: String = msg.into();
        let elapsed = self.started.elapsed();
        let line = format_final_line("✗", &self.name, &msg, elapsed, self.color);
        if self.tty {
            self.bar.finish_with_message(line);
        } else {
            eprintln!("  {line}");
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        self.resolved = true;
        // Dangling spinner — emit a generic "(cancelled)" so the line
        // doesn't keep animating after the function returned without
        // a finish call.
        let line = format!("⊘ {} (cancelled)", self.name);
        if self.tty {
            self.bar.finish_with_message(line);
        } else {
            eprintln!("  {line}");
        }
    }
}

/// Build one resolved line. Public so tests can poke at it without
/// constructing a full Spinner.
fn format_final_line(
    glyph: &str,
    name: &str,
    summary: &str,
    elapsed: std::time::Duration,
    color: crate::cli::resolve::ColorMode,
) -> String {
    let body = if summary.is_empty() {
        format!("{glyph} {name}")
    } else {
        format!("{glyph} {name} {summary}")
    };
    let body = if elapsed > std::time::Duration::from_millis(200) {
        format!("{body} ({:.1}s)", elapsed.as_secs_f32())
    } else {
        body
    };
    match glyph {
        "✓" => crate::cli::resolve::colorize_success(&body, color),
        "⚠" => crate::cli::resolve::colorize_warning(&body, color),
        "✗" => crate::cli::resolve::colorize_error(&body, color),
        _ => body,
    }
}
```

The four `colorize_*` helpers (`colorize_header`, `colorize_final_ok`, `colorize_success`, `colorize_warning`, `colorize_error`) will be added in Step 2. They wrap the existing `cli::resolve::ColorMode` and `cli::resolve::colorize_*` infrastructure where possible.

- [ ] **Step 2: Add the color helpers in `cli::resolve`**

`src/cli/resolve.rs` already exposes `colorize_header(text, mode)` and `colorize_prompt(text, mode)`. Add three more (place next to the existing ones):

```rust
pub fn colorize_success(text: &str, mode: ColorMode) -> String {
    match mode {
        ColorMode::Color => format!("\x1b[32m{text}\x1b[0m"),  // green
        ColorMode::Plain => text.to_string(),
    }
}

pub fn colorize_warning(text: &str, mode: ColorMode) -> String {
    match mode {
        ColorMode::Color => format!("\x1b[33;1m{text}\x1b[0m"),  // bold amber
        ColorMode::Plain => text.to_string(),
    }
}

pub fn colorize_error(text: &str, mode: ColorMode) -> String {
    match mode {
        ColorMode::Color => format!("\x1b[31;1m{text}\x1b[0m"),  // bold red
        ColorMode::Plain => text.to_string(),
    }
}

pub fn colorize_final_ok(text: &str, mode: ColorMode) -> String {
    match mode {
        ColorMode::Color => format!("\x1b[32;1m{text}\x1b[0m"),  // bold green
        ColorMode::Plain => text.to_string(),
    }
}
```

Match the style of the existing `colorize_header` / `colorize_prompt`.

- [ ] **Step 3: Build to confirm compile**

```bash
cargo build
```
Expected: clean. The new code is unused but compiles.

- [ ] **Step 4: Add unit tests for `format_final_line`**

In `src/progress.rs`, append a `#[cfg(test)] mod log_tests` block (separate from the existing `mod tests` for OverallProgress):

```rust
#[cfg(test)]
mod log_tests {
    use super::*;
    use std::time::Duration;
    use crate::cli::resolve::ColorMode;

    #[test]
    fn format_final_line_short_op_omits_elapsed() {
        let line = format_final_line("✓", "workspaces", "4", Duration::from_millis(40), ColorMode::Plain);
        assert_eq!(line, "✓ workspaces 4");
    }

    #[test]
    fn format_final_line_long_op_includes_elapsed() {
        let line = format_final_line("✓", "schemas", "24", Duration::from_millis(1400), ColorMode::Plain);
        assert_eq!(line, "✓ schemas 24 (1.4s)");
    }

    #[test]
    fn format_final_line_empty_summary_just_glyph_and_name() {
        let line = format_final_line("⚠", "hooks/x", "", Duration::from_millis(50), ColorMode::Plain);
        assert_eq!(line, "⚠ hooks/x");
    }
}
```

- [ ] **Step 5: Run the tests**

```bash
cargo test -p rdc --lib progress::log_tests -- --nocapture
```
Expected: 3 passes.

Run the full suite:
```bash
cargo test -p rdc -- --nocapture
```
Expected: all 569 tests still pass (the new code is unused; the old `OverallProgress` still drives everything).

- [ ] **Step 6: Stage**

```bash
git add src/progress.rs src/cli/resolve.rs
```

Do NOT commit; the controller commits on your behalf.

---

## Task 2: Migrate all callers; delete the old API

Switch every caller from `OverallProgress` to `ProgressLog` / `Phase` / `Spinner`. Delete `OverallProgress` and its supporting machinery. Update integration tests that asserted on the old shape.

**Files modified:**
- `src/progress.rs` — delete `OverallProgress`, `Inner` enum, `ProgressHandle` type alias, the old `Drop` impl, the old `#[cfg(test)] mod tests` block.
- `src/cli/pull/{mod,common,workspaces,queues,hooks,rules,labels,engines,engine_fields,workflows,workflow_steps,email_templates,organization,mdh}.rs` — migrate per the table below.
- `src/cli/push/{mod,workspaces,queues,schemas,inboxes,email_templates,hooks,rules,labels,engines,engine_fields,deletes,scan}.rs` — same.
- `src/cli/deploy/{run,apply,create}.rs` — same.
- `src/cli/sync/{mod,execute,watch}.rs` — same.
- `tests/cli_deploy.rs`, `tests/cli_sync.rs`, `tests/cli_repair.rs`, `tests/cli_misc.rs` — update assertions that scanned for the old strings.

### Migration table (apply at every call site)

| Old | New |
|---|---|
| `progress: &Arc<OverallProgress>` | `progress: &Arc<ProgressLog>` |
| `OverallProgress::start(title)` | `ProgressLog::start(title)` |
| `progress.inc_total(n)` | **delete the call** — no denominator anymore |
| `progress.start_phase("hooks")` returns `Option<(u64, usize, Duration)>` | `let phase = progress.phase("pulling hooks");` (drops the unused return tuple) |
| `progress.tick(item_name)` | replace with `let sp = phase.item(item_name); /* work */; sp.finish_ok(summary)` at each tick site — the per-driver loop now creates a spinner per item |
| `progress.println(msg)` | `phase.line(format!("⚠ {msg}"))` for warnings, or `sp.finish_warn(msg)` if there's an in-flight spinner |
| `progress.skipped_orphan()` | `phase.line(format!("⊘ orphan skipped"))` (orphans are a per-item observation; no central counter anymore) |
| `progress.finish()` | `progress.finish(summary_str)` — caller must compose the summary, e.g. `format!("Synced envs/{env} ({n} changed, {:.1}s)", elapsed.as_secs_f32())` |
| `progress.orphans()` / `progress.total_processed()` | **delete** — counters move to the caller if needed; the final summary string is composed locally |

### Step-by-step

- [ ] **Step 1: Replace `src/progress.rs` body**

Replace the entire contents of `src/progress.rs` with just the new API (everything we added in Task 1 Step 1), plus the module-level imports trimmed to what the new code actually uses:

```rust
//! Per-run UX during pull/push/deploy/sync — colored event log with
//! per-line spinners while individual operations are in flight.
//!
//! Spec: docs/superpowers/specs/2026-05-15-progress-log-design.md

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use indicatif::{ProgressBar, ProgressStyle};

// ... paste the new types (`ProgressLog`, `Phase`, `Spinner`,
//     `format_final_line`) from Task 1 here, unchanged ...

// Keep the log_tests module.
```

The old `OverallProgress` and `Inner` enum are gone. The `ProgressHandle` type alias is gone.

- [ ] **Step 2: Build to surface the breakage**

```bash
cargo build 2>&1 | head -80
```
Expected: many errors of the form "no associated function `start` for type `OverallProgress`" or "no method `tick` on `ProgressLog`". Every error points at a migration site.

- [ ] **Step 3: Migrate the pull drivers**

Start with `src/cli/pull/common.rs`. Find the `PullCtx` struct — if it carries `progress: Arc<OverallProgress>` (or similar), rename the type to `ProgressLog`. Same in `src/cli/pull/mod.rs::PullCtx` if present.

For each per-kind file (`src/cli/pull/{workspaces,queues,hooks,rules,labels,engines,engine_fields,workflows,workflow_steps,email_templates,organization,mdh}.rs`):

1. Find `pub async fn process(... progress: &Arc<OverallProgress>) -> Result<(usize, usize)>` and change the param type to `&Arc<ProgressLog>`.
2. Find the per-item loop. Today it looks like:
   ```rust
   for item in &items {
       // ... work ...
       progress.tick(&item.name);
   }
   ```
   Replace with:
   ```rust
   let phase = progress.phase("pulling labels");  // or whatever the kind is
   for item in &items {
       let sp = phase.item(&item.name);
       // ... work ...
       sp.finish_ok("");  // or with a brief summary if useful
   }
   ```
   The phase is started once per `process` call. Inside the loop, each item gets its own spinner that resolves to `✓` on completion.
3. Remove any `progress.inc_total(...)` call.
4. Remove any `progress.start_phase(...)` call (the `phase()` call above replaces it).
5. Replace `progress.skipped_orphan()` with `phase.line("⊘ orphan: <slug>")` if you have a slug to name, otherwise drop it.
6. After the loop, no explicit finalize is needed for the phase — the next `phase()` call starts a fresh one, or the run's `finish()` ends everything.

Repeat for every pull driver. The pattern is identical; just the kind name in `phase("pulling <kind>")` and the per-item summary differ.

- [ ] **Step 4: Migrate the push drivers**

Same shape for `src/cli/push/{workspaces,queues,schemas,inboxes,email_templates,hooks,rules,labels,engines,engine_fields,scan,deletes,mod}.rs`. The push side has clearer per-object lines (PATCH/POST/DELETE), so the spinner summary is more informative:

```rust
let sp = phase.item(format!("{kind}/{slug}"));
let res = client.patch(...).await;
match res {
    Ok(_) => sp.finish_ok(format!("→ PATCH")),    // elapsed appended automatically
    Err(e) => sp.finish_err(format!("PATCH failed: {e}")),
}
```

In `push/mod.rs::push_classified`, the top-level loop owns the `phase("pushing")` and per-kind it just keeps calling `phase.item(...)`. Per-kind driver functions can take `phase: &Phase` instead of `progress: &Arc<ProgressLog>` for the inner loop — that's a cleaner shape than re-creating phases.

(Alternative if changing per-kind signatures is too invasive: keep the `progress: &Arc<ProgressLog>` and let each per-kind driver call its own `phase()` early on. Pick whichever is less code churn.)

`push/deletes.rs` is structurally similar; the destructive gate's confirmation prompts (`confirm_or_refuse`) stay unchanged — they're separate stderr writes.

- [ ] **Step 5: Migrate the deploy drivers**

`src/cli/deploy/{run,apply,create}.rs`. Deploy has the most phases:

- "auto-mapping" (small one-shot — use `phase("auto-mapping")` then `phase.line("paired N objects")`)
- "create sweep" (per-kind, with per-object lines for creates)
- "update sweep" (similar)
- "delete sweep" (under `--mirror`)

For each existing `progress.start_phase("creating workspaces")` → `let phase = progress.phase("creating workspaces");`. For each `progress.tick(slug)` → `let sp = phase.item(slug); ... sp.finish_ok("POST")` (or `"PATCH"` / `"DELETE"`).

- [ ] **Step 6: Migrate the sync executor**

`src/cli/sync/{mod,execute}.rs`. The cycle pipeline has phases: list remote, scan local, classify, conflict resolution, pull-side writes, push-side writes, save lockfile.

In `sync::run_cycle`, replace `OverallProgress::start("sync envs/{env}")` with `ProgressLog::start("rdc sync {env}")`. Replace each `start_phase("listing remote")` etc. with `let phase = log.phase("listing remote");`.

The cycle counters that today come from `progress.total_processed()` / `progress.orphans()` are now composed by the caller — accumulate them into local `let mut pushed = 0;` variables and feed into `log.finish(format!("Synced envs/{env} ({pushed} changed, {:.1}s)", elapsed.as_secs_f32()))`.

- [ ] **Step 7: Migrate the watch loop**

`src/cli/sync/watch.rs`. Watch's outer loop already prints its own per-cycle lines (`[14:02:17] → cycle: ...`) and doesn't use `OverallProgress`. The INNER cycle (calling `run_cycle`) uses the new `ProgressLog`. Verify that the cycle's per-line output doesn't visually clash with the watch loop's wrapper line — if it does, suppress the cycle's header/footer by passing a quieter flag, or just live with the slight nesting.

For now: no behavior change to the watch loop itself. The inner cycle's output goes to stderr alongside watch's existing summary line.

- [ ] **Step 8: Update integration tests**

Search for old-shape assertions:

```bash
grep -rn "→ phase\|✓ phase\|↳ \|discovering items\|N items, " tests/ src/ 2>&1 | head -40
```

For each hit:
- If the assertion is on the **count of operations** (e.g., "2 items in this phase"), reframe as: count specific final lines in the new output (e.g., `assert!(stdout.contains("hooks/x PATCH"))` instead of `assert!(stdout.contains("✓ hooks: 2 items"))`).
- If the assertion is on **a specific message string**, update to the new shape.
- If the test only cared that the run finished cleanly, replace the assertion with `assert!(stdout.contains("✔ Synced"))` or similar.

Be conservative: don't delete assertions, transform them.

Two specific tests to watch for:
- `tests/cli_sync.rs::sync_no_push_and_no_pull_together_errors` — asserts on error message wording. Unaffected (error path doesn't go through progress).
- `tests/cli_deploy.rs::deploy_waits_for_tgt_env_lock` — asserts on elapsed time, not strings. Unaffected.

- [ ] **Step 9: Build clean**

```bash
cargo build 2>&1 | tail -20
```
Expected: clean. Zero compile errors. Zero unused-import warnings.

- [ ] **Step 10: Run the suite**

```bash
cargo test -p rdc -- --nocapture 2>&1 | tail -40
```
Expected: all 569+ tests pass. The new unit tests in `progress::log_tests` (3 from Task 1) stay green; integration tests with updated assertions pass.

If a test fails with "no such substring", the assertion needs updating to match new output. Update conservatively.

- [ ] **Step 11: Manual smoke test**

```bash
# In a separate tab, run a smoke against any project dir:
cargo run -- sync test --dry-run 2>&1 | head -30
```
Confirm the output matches the spec's example shape:
- `→ rdc sync test` header
- `listing remote` phase header
- `  workspaces  ✓ 4` per-kind summaries
- Final `✔ Synced envs/test` line

If the layout is misaligned (e.g. wrong indent), tune the format strings in `src/progress.rs`.

- [ ] **Step 12: Stage**

```bash
git add -A
```

Do NOT commit; the controller commits on your behalf.

---

## Task 3: README touch + cleanup

Quick scan for README references to the old bar; touch as needed.

**Files:**
- Modify: `README.md` (small).

- [ ] **Step 1: Search the README for stale output examples**

```bash
grep -n "↳\|discovering items\|wide_bar\|ProgressBar\|[wide_bar]" README.md
```

If any example shows the old bar's `[wide_bar]` template or `↳ message` shape, replace with the new event-log shape. Most likely candidates: the "Promote test → prod" subsection has a sample deploy output; verify it matches reality.

If no hits, skip this task entirely. The README's `60-second tour` only shows the summary line (`✔ Synced ...`), which the new code preserves.

- [ ] **Step 2: Quick visual scan**

Run a manual smoke (`cargo run -- sync test --dry-run` against a scratch project) and compare against any README snippets. Adjust README snippets to match.

- [ ] **Step 3: Run the suite**

```bash
cargo test -p rdc -- --nocapture
```
Expected: PASS — no source changes here.

- [ ] **Step 4: Stage**

```bash
git add README.md
```

If no changes were needed, skip the staging.

---

## Spec self-review

Walk through `docs/superpowers/specs/2026-05-15-progress-log-design.md` and confirm coverage:

- **Rendered output (TTY)** — Task 1 Step 1 (spinner template + final-line formatter); Task 2 Steps 3-7 (every driver migrated to per-kind / per-object lines).
- **Rendered output (non-TTY)** — Task 1 Step 1 (`indicatif::ProgressBar::hidden()` path + direct `eprintln!` on finish).
- **Color palette** — Task 1 Step 2 (`colorize_success` / `colorize_warning` / `colorize_error` / `colorize_final_ok`).
- **Spinner mechanics** — Task 1 Step 1 (`new_spinner` + `enable_steady_tick(80ms)` + `dots` glyph set).
- **New API surface** — Task 1 Step 1.
- **Migration mapping table** — Task 2 (preamble table + Steps 3-7).
- **Watch mode interaction** — Task 2 Step 7.
- **Layered file structure** — Task 1 Step 1 (single file).
- **Testing (unit)** — Task 1 Step 4 (`format_final_line` tests).
- **Testing (integration)** — Task 2 Step 8 (assertion updates).
- **CI / non-TTY behavior** — Task 1 Step 1 (`ProgressBar::hidden()` branch).
- **Edge cases (spinner outlives function)** — Task 1 Step 1 (`Drop for Spinner`).
- **Edge cases (errors mid-phase)** — Task 2 Steps 3-7 use `finish_err` per driver.
- **Open questions (spinner delay, kind hint)** — explicitly out of scope; revisit if user feedback says so.

If any spec requirement is not pointed at by a step, add a step. If the plan has placeholder text, fix it inline.
