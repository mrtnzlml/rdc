# Progress Log — Design

**Status:** Spec, awaiting review
**Date:** 2026-05-15
**Scope:** Replace the `indicatif` progress bar used by `rdc pull` / `rdc push` / `rdc deploy` / `rdc sync` with a line-by-line event log. Reads are summarized per kind; writes get one line per object. Long-running waits show an inline spinner that resolves to a final ✓ when the work completes. CI / non-TTY emits the same line content without ANSI animation.

## Goal

The current bar (`{spinner} {prefix} [{wide_bar}] {pos}/{len} ETA {eta}\n  ↳ {msg}`) hides what's actually happening. Users see a fraction tick up; if the bar stalls, they can't tell whether it's hung or just chewing on a slow object. Replace with a stream of events that says exactly what rdc is doing — and shows a spinner so it's obvious when rdc is actively waiting on something.

## Non-goals

- **Per-line timestamps in one-shot commands.** Watch mode keeps its `[HH:MM:SS]` prefixes (a continuously-running daemon benefits from them). One-shot commands run for a few seconds; timestamps are noise.
- **A new dependency.** `indicatif` is already in the tree and supports spinners directly; reuse it. No replacing.
- **`MultiProgress` / parallel spinner stacks.** Today's drivers serialize their visible work. If a future refactor parallelizes, that's a separate design.
- **Configurable verbosity flags right now.** Watch already has `-v`; the rest of the commands get the new default and we can layer a flag later if needed.
- **Custom theming.** Reuse the existing amber-accent palette from `src/cli/mod.rs::CLI_STYLES`.

## Background

`src/progress.rs` (282 lines) defines `OverallProgress`. Two modes:

- **`Bar`** (TTY, `std::io::stderr().is_terminal()`): single `indicatif::ProgressBar` with the template above. Steady-tick at 100 ms. `inc_total(n)` extends the denominator; `start_phase(name)` swaps the sub-label; `tick(item)` bumps the position and updates `↳ {item}` underneath.
- **`Log`** (CI / piped): emits `→ phase: …` on phase start, `✓ phase: N items, Xs` on phase end. No bar. The current integration tests assert on these exact strings.

Callers (16 source files, ~340 references): every per-kind driver in `cli::pull::*`, `cli::push::*`, `cli::deploy::{run,apply,create}`, and `cli::sync::execute`. The shared call patterns are:

1. `OverallProgress::start("pull envs/dev")` → cloneable `Arc<Self>` handed to drivers.
2. Phase 1 (listing): each driver calls `inc_total(n)` after listing its kind.
3. Phase 2 (processing): each driver calls `start_phase("hooks")` then `tick(slug)` per object.
4. `progress.finish()` at the end of the run.

The migration must keep the call-site shape ergonomic while replacing the rendering.

## Design

### Rendered output (TTY)

```
$ rdc sync test
→ rdc sync test

listing remote
  workspaces        ✓ 4
  queues            ✓ 24
  schemas           ⠙ (12.3s)        ← spinner while waiting on a paginated GET
  schemas           ✓ 24
  hooks             ✓ 27
  rules             ✓ 1
  labels            ✓ 46

scanning local      ✓ 2 changes
classifying         ✓ 2 push, 0 pull, 0 conflicts

pushing
  hooks/validator-invoices  → PATCH ✓ 0.4s
  rules/finance-totals      → PATCH ✓ 0.6s

✔ Synced envs/test (2 pushed, 1.4s)
```

- **Phase headers** (`listing remote`, `pushing`) on their own lines, no leading symbol.
- **Per-kind summaries** indented under "listing remote" — one line per kind.
- **Per-object lines** indented under "pushing" / "pulling" — one line per write.
- **Spinner** replaces the line content while a single operation is in flight. When the op finishes, the spinner glyph swaps to `✓` (success), `⚠` (warning), or `✗` (error); elapsed time is appended for ops that took > 200 ms.
- **Final summary** as `✔ Synced envs/test (2 pushed, 1.4s)` (bold + green).

### Rendered output (non-TTY)

Same line *content*, no animation. Each line emitted on completion (or on start for phase headers, since there's nothing to animate). Lines remain stable and grep-friendly so CI logs and `tee`-d output read the same as the terminal. The existing test fixtures asserting on `→ phase: …` and `✓ phase: N items, Xs` get retired; new fixtures assert on the new shapes.

### Color palette

Reuse `src/cli/mod.rs`'s established palette:

| Element | Color | Effect |
|---|---|---|
| Phase header (`listing remote`) | amber | bold |
| Spinner glyph (`⠙` …) | amber | normal |
| Success `✓`, final `✔` | green | normal / bold |
| Warning `⚠` | amber | bold |
| Error `✗` | bright red | bold |
| Elapsed time, counts | gray | normal |
| Object slugs, paths | default | normal |

Disabled when `NO_COLOR` is set, `--no-color` is passed, or stderr isn't a TTY (existing detection in `cli::resolve::detect_color_mode`).

### Spinner mechanics

`indicatif::ProgressBar::new_spinner()`, with `enable_steady_tick(Duration::from_millis(80))`. Frame set: the default `dots` (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`). Each `Spinner` handle owns one ProgressBar and finishes-and-clears (or finishes-with-message) on resolution.

In non-TTY mode, `new_spinner()` is suppressed — we just emit the final line on `finish_ok` / `finish_err`. (`indicatif` already auto-detects no-TTY and skips the tick loop.)

### New API

Replaces `OverallProgress` entirely:

```rust
// src/progress.rs

pub struct ProgressLog { /* ... */ }

impl ProgressLog {
    /// Print the header line and return the handle. `title` becomes
    /// the run header (`→ rdc sync test`).
    pub fn start(title: impl Into<String>) -> Arc<Self>;

    /// Start a labelled section. The label appears on its own line.
    /// Subsequent `item()` calls are indented under it. A second `phase()`
    /// call finalizes the previous phase (blank line) and starts a new one.
    pub fn phase(&self, label: impl Into<String>) -> Phase;

    /// Final summary line (`✔ Synced envs/test (2 pushed, 1.4s)`).
    /// Called once at the end.
    pub fn finish(&self, summary: impl Into<String>);

    /// Final summary on error (`✗ Sync failed: <msg>`). Emits then returns;
    /// the caller is expected to propagate the error.
    pub fn finish_err(&self, msg: impl Into<String>);
}

pub struct Phase<'a> { /* borrows from ProgressLog */ }

impl<'a> Phase<'a> {
    /// Start a single item. Returns a `Spinner` that owns the line until
    /// it's finished.
    pub fn item(&self, name: impl Into<String>) -> Spinner;

    /// One-shot summary line without a spinner (for scan/classify steps).
    pub fn line(&self, content: impl Into<String>);
}

pub struct Spinner { /* owns one indicatif ProgressBar */ }

impl Spinner {
    /// Update the in-flight message (rare; most callers don't need this).
    pub fn set_message(&mut self, msg: impl Into<String>);

    /// Resolve with a ✓ and optional summary, like `✓ 4` or `→ PATCH ✓ 0.4s`.
    pub fn finish_ok(self, summary: impl Into<String>);

    /// Resolve with a ⚠ and a one-line warning.
    pub fn finish_warn(self, msg: impl Into<String>);

    /// Resolve with a ✗ and a one-line error.
    pub fn finish_err(self, msg: impl Into<String>);
}
```

### Migration mapping

Old → new at the call sites:

| Old | New |
|---|---|
| `OverallProgress::start(title)` | `ProgressLog::start(title)` |
| `progress.inc_total(n)` (Phase 1 listing) | **delete** — no denominator anymore |
| `progress.start_phase("hooks")` (Phase 2) | `let phase = log.phase("pulling hooks");` |
| `progress.tick(slug)` | `let sp = phase.item(slug); /* work */ sp.finish_ok(summary);` |
| `progress.println(warn)` | `phase.line(format!("⚠ {warn}"))` or via Spinner::finish_warn |
| `progress.finish()` | `log.finish(summary_str)` |

The two structural changes worth flagging:

1. **No denominator.** `inc_total` and the `pos/len` rendering go away. The bar is gone; nothing to count toward.
2. **Tick → Spinner.** `tick(slug)` becomes "create a spinner for this slug, do the work, finalize the spinner." The callers gain explicit start/finish points, which is good for visibility but does mean every existing `tick` call site changes.

The pull/push drivers will see the most churn — each `tick(item.name)` becomes a borrow-checker-friendly `let sp = phase.item(...); ... sp.finish_ok(...);` pattern.

### Watch mode interaction

Watch mode (`src/cli/sync/watch.rs`) already prints `[HH:MM:SS] → cycle: …` summary lines per cycle and does NOT use `OverallProgress`. Keep that. Per-cycle work inside the watch loop (which DOES call into `sync::run_cycle`) will produce ProgressLog output via the new API; the watch loop captures-and-summarizes that as it does today. No watch-mode behavior change.

### Layered file structure

- `src/progress.rs` — `ProgressLog`, `Phase`, `Spinner`. Public API is just these three types plus a tiny color helper.
- Tests in `mod tests` covering the four resolution paths (`finish_ok`, `finish_warn`, `finish_err`, drop-without-finish) and the TTY vs non-TTY divergence.

### Testing

Output is fundamentally side-effecty (stderr writes, ANSI escapes). The pragmatic path:

1. **Unit tests** (in `src/progress.rs`): inject a `Write` sink instead of stderr. Run a small scripted sequence (`start`, `phase`, `item`, `finish_ok`, `finish`). Assert the captured bytes contain the expected substrings (`"listing remote"`, `"✓ 4"`, `"Synced envs/test"`). Don't assert on exact byte sequences — ANSI escapes are brittle.

2. **Integration tests** (existing `tests/cli_pull.rs`, etc.): the previous fixtures asserted on `→ phase: …` strings. Replace with assertions on the new shapes. Use the non-TTY rendering — that's what the test harness sees anyway (no real terminal).

3. **No snapshot tests of full output.** ANSI cursor sequences shift between `indicatif` versions; we test for content presence, not byte layout.

### CI / non-TTY behavior

`indicatif` already auto-detects no-TTY and skips the steady-tick loop. The line content is the same; the spinner glyph just never appears (the final line is emitted on resolve directly). For logs piped to a file, the user sees an in-order event log with no carriage returns.

Detection: `std::io::stderr().is_terminal() && !env::var("NO_COLOR").is_ok_and(|v| !v.is_empty())`. Same shape `cli::resolve` uses.

### Edge cases

- **Spinner outlives the function**: if `finish_ok` isn't called and the `Spinner` drops, finalize with a generic "(cancelled)" so the line doesn't dangle. Implement via `Drop`.
- **Errors mid-phase**: caller emits `sp.finish_err(msg)` and the function returns the error. The Phase header stays; subsequent items don't start. The final line should be `log.finish_err(...)`.
- **Empty phase**: phase header is printed, then no items, then next phase. Acceptable — the header acts as a checkpoint.
- **Very fast operations** (sub-50ms): spinner appears and disappears in one tick. The `finish_ok` line is still emitted. User sees a brief flicker; acceptable. (Alternative: delay spinner display by 200ms — adds complexity without clear UX win. Skip for v1.)

## Code shape

**Delete:**
- The `Inner::Bar` variant of `OverallProgress` (the bar branch).
- `inc_total` (no callers after migration).
- `phase_count` / `total_count` accumulators (the new API doesn't display them).
- The bar template string.

**Replace:**
- `src/progress.rs` fully — keep the file name but change the types.
- Every caller in the 16 affected files. The migration is mechanical per the table above; touch each driver in one focused commit.

**Keep:**
- The `Arc<ProgressLog>` clone pattern — drivers still need a cloneable handle.
- The mutex around shared mutable state (the active phase, the spinner handle).
- Existing color detection (`cli::resolve::detect_color_mode`).

### Testing strategy summary

- New unit tests in `src/progress.rs` (capture into a `Vec<u8>` sink; assert on content substrings).
- Update existing integration test assertions that scanned for the old `→ phase: …` / `✓ phase: ...` strings.
- Add one snapshot-ish integration test that runs `cargo run -- sync test --dry-run` (with mocks) and greps the captured stderr for: phase headers, per-kind summary lines, final `✔ Synced`.

## Compatibility

Internal change only. Affects:

- **Console output** for `pull`, `push`, `deploy`, `sync` (one-shot and watch's per-cycle work).
- **No public APIs** change. `OverallProgress` was crate-internal.
- **CI log scrapers** that grep for the old `→` / `✓` strings will need updating. Document in commit message.

## Open questions

- **Single-line vs two-line rendering for in-flight items.** Today's draft: one line per item, spinner replaces content in place, then resolves to ✓. Alternative: two lines (header + indented status), heavier visually. v1 sticks with one line.
- **Should `phase.item()` accept a "kind" hint** so it can render `hooks/validator-invoices` with the slug part highlighted? Easy follow-up; not needed for v1.
- **Spinner delay (200 ms before showing)** for fast ops — would reduce flicker but adds complexity. v1 shows immediately; revisit if user feedback says it's noisy.

## Out of scope (deferred)

- **`--verbose` / `--quiet` flags** for one-shot commands. Watch mode has `-v`; the rest can match later if there's demand.
- **Per-line timestamps in one-shot commands.** Watch mode keeps them.
- **Parallel spinner stacks** (`MultiProgress`). Drivers serialize today; revisit if/when work parallelizes.
- **JSON / structured-event output mode.** Out of scope; rdc is interactive-first.
- **Replacing the indicatif dependency.** Already in the tree, works fine for spinners.
