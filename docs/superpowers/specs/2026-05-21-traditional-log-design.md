# Traditional Log — Design

**Status:** Spec, awaiting review
**Date:** 2026-05-21
**Scope:** Replace the grid renderer (`src/progress/grid.rs`) and the line-based `ProgressLog` (`src/progress/log.rs`) with a single timestamped event-log renderer used by every `rdc` command. Every `println!` / `eprintln!` / `phase` / `warn_line` callsite in the application is migrated to the new format.

## Goal

The grid hides what `rdc` is doing behind a wall of colored squares. Today's `ProgressLog` is closer to a log but mixes phase headers, indented item lines, and live spinners; it doesn't carry timestamps in one-shot commands; and the per-command output style varies across `pull`, `push`, `deploy`, `sync`, `auth`, `diff`, and `upgrade`.

Replace both with one renderer that emits **one timestamped line per event**, in the same shape everywhere, with a closed vocabulary of action verbs and a fixed column layout. The result reads the same on a TTY and in `tee`d / CI output; it scrolls past cleanly during long runs; and `grep` works.

## Non-goals

- **No grid renderer.** The grid is deleted, not hidden behind a flag.
- **No spinners.** Long-running ops emit a `start` event and a completion event; no animation.
- **No new dependencies.** Stay on `std`; `indicatif` is removed (assuming no remaining consumer — verify during implementation).
- **No per-event configurable verbosity.** Watch keeps its existing `-v`; everything else gets the new default.
- **No structured / JSON log mode.** Future work if it materializes.
- **No multi-line plan/dry-run block restructuring.** The big JSON-body and conflict-prompt blocks stay as multi-line documents — they're emitted untimestamped via a `block()` helper, sandwiched between timestamped lifecycle events.

## Rendered output

```
12:01:14 sync   start envs/test
12:01:14 list   workspaces start
12:01:15 list   workspaces (4, 0.4s)
12:01:15 list   schemas start
12:01:27 list   schemas (24, 12.3s)
12:01:27 push   rule/finance-totals PATCH 412ms
12:01:28 skip   rule/sla-check (drift)
12:01:28 retry  POST hooks 429 (waiting 2s)
12:01:30 post   hook/validator-invoices id=4711
12:01:30 warn   inbox/foo lockfile entry has no content_hash
12:01:30 sync   done (1 pushed, 1 skipped, 16.4s)
```

TTY and non-TTY output is **byte-identical** apart from ANSI color codes (which `ColorMode::Plain` / `NO_COLOR` strip).

## Line format

```
HH:MM:SS  <action>  <body>
```

- **Time** — UTC, `HH:MM:SS`, dim. Reuse the existing `now_hhmmss` from `src/cli/sync/watch.rs` and promote it to a shared module (e.g. `src/log.rs` itself).
- **Action** — a closed enum (see below). Rendered left-padded to **6 characters** + 1 space separator. Vertical alignment of the body column is the visible contract.
- **Body** — free-form. Convention is `<kind>/<slug>  <detail>` for per-resource events and free text otherwise.

### Action vocabulary

A single `enum Action` with the variants below. No string-typed actions are allowed at callsites; adding a new action is a deliberate enum edit.

| Category | Variants | Notes |
|---|---|---|
| Run lifecycle | `Sync` (`sync`), `Deploy` (`deploy`), `Pull` (`pull`), `Push` (`push`), `Diff` (`diff`), `Auth` (`auth`), `Init` (`init`), `Repair` (`repair`), `Upgr` (`upgr`) | `upgrade` is abbreviated to fit 6 chars |
| Phase | `Plan` (`plan`), `List` (`list`), `Watch` (`watch`) | |
| Per-resource | `Post` (`post`), `Patch` (`patch`), `Delete` (`delete`), `Skip` (`skip`) | |
| Disposition | `Done` (`done`), `Fail` (`fail`), `Warn` (`warn`), `Retry` (`retry`), `Info` (`info`) | |
| Watch | `Tick` (`tick`), `Idle` (`idle`) | |

### Color palette

Reuses `cli::resolve::ColorMode` and the existing `colorize_*` helpers; respects `NO_COLOR` and non-TTY automatically.

| Element | Color | Effect |
|---|---|---|
| Time prefix | dim | |
| Action column — lifecycle / phase / per-resource | amber | accent |
| Action — `done` | green | |
| Action — `warn`, `retry` | yellow | |
| Action — `fail` | red | bold |
| Action — `skip`, `idle`, `info` | dim | |
| Body | default | |

### Body conventions

- Per-resource: `<kind>/<slug> [<HTTP method>] [<detail>] [<elapsed>]` — e.g. `rule/finance-totals PATCH 412ms`, `hook/validator-invoices id=4711`.
- Listing summary: `<kind> (<count>[, <elapsed>])` — e.g. `workspaces (4, 0.4s)`.
- Start of a long op: `<subject> start` — paired with a later completion event using the same action.
- Run lifecycle: `start <args>` / `done (<summary>)` / `fail <reason>` as the body, with the run command as the action — e.g. `12:01:14 sync   start envs/test`.

Elapsed time is appended only for ops that took ≥ 200 ms (matches today's `format_final_line` rule). Resource id / HTTP status / row counts are written in the body verbatim.

## Architecture

### Module layout

```
src/
  log.rs                ← new: Log struct, Action enum, time helper, color routing
  progress/             ← deleted (mod.rs, log.rs, grid.rs — ~1500 LOC)
```

### Public surface

```rust
// src/log.rs

#[derive(Copy, Clone)]
pub enum Action { /* see vocabulary table */ }

impl Action {
    /// 6-char left-padded lowercase token (e.g. "sync  ", "patch ", "delete").
    pub fn pad(self) -> &'static str;
}

pub struct Log {
    color: ColorMode,
    out:   Mutex<BufWriter<Stderr>>,
}

impl Log {
    pub fn new(color: ColorMode) -> Arc<Self>;

    /// Emit one timestamped event line to stderr.
    pub fn event(&self, action: Action, body: &str);

    /// Emit a multi-line untimestamped block (plan dumps, JSON bodies,
    /// conflict-prompt UI). Trailing newline is added if missing.
    pub fn block(&self, body: &str);

    /// Run an inline prompt (auth refresh, conflict resolver, delete gate).
    /// With no spinners to suspend this is currently a passthrough; kept
    /// as a method so callsites still express intent and we can add
    /// flushing later if needed.
    pub fn with_prompt<F, T>(&self, f: F) -> T where F: FnOnce() -> T;
}
```

No trait. One concrete struct. `Arc<Log>` is threaded through every command exactly as `Arc<dyn SyncRenderer>` is today — callsite *shape* is preserved even though the API surface changes.

### Why no trait

The trait existed to abstract grid vs log. With one renderer, the trait is dead weight. If a future renderer (e.g. JSON log) is needed, reintroduce a trait at that time — YAGNI for now.

### Threading

`Log` is `Send + Sync` via the `Mutex<BufWriter<Stderr>>`. Every command currently holds an `Arc` of the renderer; that pattern is unchanged. Concurrent emitters serialize at the writer.

### Time helper

`fn now_hhmmss() -> String` moves from `src/cli/sync/watch.rs` into `src/log.rs`. Watch keeps no private copy. No `chrono` dependency is added.

## Callsite migration

Roughly 30 source files emit user-facing lines. The mechanical translation:

| Today | Becomes |
|---|---|
| `progress.phase("pushing engines")` | (removed — phase is implicit in the action column) |
| `progress.warn_line("[ok] engines/x PATCH")` | `log.event(Patch, "engine/x")` |
| `progress.warn_line("! engines/x skipping (drift)")` | `log.event(Skip, "engine/x (drift)")` |
| `sp.finish_ok("4 fetched")` (per-kind list spinner) | `log.event(List, "workspaces (4, 0.4s)")` |
| `progress.finish("Synced envs/test")` | `log.event(Sync, "done (1 pushed, 4.2s)")` |
| `progress.finish_err("token failed")` | `log.event(Auth, "fail token validation")` |
| `eprintln!("[{}] cycle failed: {e:#}", now_hhmmss())` (watch) | `log.event(Fail, &format!("watch cycle: {e:#}"))` |
| `println!("Plan: test -> prod\n  + create: ...")` (deploy `--dry-run`) | `log.event(Plan, "test -> prod (12 create, 0 patch)"); log.block("  + create: ...\n  ...")` |
| `eprintln!("error: {err:#}")` in `src/main.rs` | `log.event(Fail, &format!("{err:#}"))` |

### Per-driver listing-summary translation

Drivers that today emit `[ok] workspaces 4 (0.4s)` via a spinner translate to:

```rust
log.event(Action::List, "workspaces start");
// ... do work ...
log.event(Action::List, &format!("workspaces ({n}, {elapsed:.1}s)"));
```

`src/cli/diff.rs` uses spinner `set_message` to show live counters (`schemas (12/24)`). Without spinners, intermediate counts disappear — only the start and completion events are emitted. Counters were a UX nicety, not a correctness signal; this is an accepted UX regression in service of format consistency.

### Retry chatter

`src/api/retry.rs` takes `&Log` instead of `ProgressHandle` and calls:

```rust
log.event(Action::Retry, &format!("{method} {path} {status} (waiting {wait_s}s)"));
```

### Plan / dry-run blocks

`src/cli/deploy/run.rs` writes a multi-line plan body (`Plan: test -> prod / Selected: / + create: / -- create bodies --` + JSON dumps). These stay as multi-line documents via `log.block()`. Lifecycle around them is timestamped:

```
12:03:01 plan   test -> prod (12 create, 0 patch)
Plan: test -> prod
  Selected:
    hooks/foo
  + create:  hooks (3), rules (1)
  ~ patch:   queues (2)
  -- create bodies (would-be POST) --
  { ... }
12:03:02 plan   accepted
12:03:02 post   hook/validator-invoices id=4711
12:03:02 deploy done (12 created, 1.8s)
```

Conflict-prompt UI in `src/cli/sync/execute.rs` follows the same pattern: a timestamped `plan` (or `sync`) lifecycle line, then an untimestamped interactive block via `log.block()`, then a follow-up event with the user's decision (`sync   conflict resolved (kept local)` etc.).

## Removals

- `src/progress/` directory in full (`mod.rs`, `log.rs`, `grid.rs`).
- `SyncRenderer` trait, `make_sync_renderer` dispatcher.
- `Phase`, `Spinner`, `ProgressHandle`, `ResourceOp`, `ResourceOutcome`, `Severity` types.
- `grid::detect_color_depth` and `grid::ColorDepth` (superseded by `cli::resolve::ColorMode`).
- `indicatif` dependency in `Cargo.toml` (verify no remaining consumer first; fall back to keeping it if there is).
- `[HH:MM:SS]`-bracket watch prefix in `src/cli/sync/watch.rs` (replaced by the standard line format).
- `docs/superpowers/specs/2026-05-15-progress-log-design.md` and `docs/superpowers/specs/2026-05-20-sync-grid-visualization-design.md` get a `superseded by 2026-05-21-traditional-log-design.md` note at the top (not deleted — historical context).

## Testing

Unit tests in `src/log.rs`:

- `event_line_format` — `12:01:14 push   hook/x PATCH 412ms`-shape line, with a fixed time injected.
- `action_column_padding` — every `Action` variant's `pad()` returns a 6-char string.
- `body_column_alignment` — across all variants, position of first body char is identical.
- `block_passes_through_unchanged` — input string emerges verbatim (with trailing `\n` added if absent).
- `no_color_strips_ansi` — under `ColorMode::Plain`, no ANSI codes appear.
- `concurrent_writers_dont_interleave` — two threads emitting 100 events each produce 200 unbroken lines.

Existing snapshot tests in `src/progress/log.rs::log_tests` (the `[ok] name summary` shape) are retired.

Integration tests in `tests/` will fail on output-shape assertions. Updating them is bundled into the migration commit for each driver — each commit stays self-contained and reviewable.

## Risk callouts

1. **`src/cli/sync/execute.rs`** has the densest concentration of `warn_line` calls (~30) plus intricate conflict-resolution branches. Highest migration cost.
2. **`src/cli/diff.rs`** loses live counter UX. Accepted regression.
3. **Integration test fixtures** in `tests/` need bulk updates — pure string-match churn. Suggest a separate commit per driver to keep diffs reviewable.
4. **`indicatif` removal** is dependent on no other consumer surviving. If something else uses it, it stays — not a blocker.
5. **Watch-mode users** lose their `[HH:MM:SS]` bracket convention. Acceptable since the new format already carries `HH:MM:SS` in the standard column.

## Migration order (implementation plan will sequence this)

The detailed plan is produced by `superpowers:writing-plans` next. At a high level:

1. Land `src/log.rs` with `Log`, `Action`, and unit tests. No callsites yet.
2. Add `Log` alongside the existing renderer (parallel surface) so commands can be migrated one at a time.
3. Migrate `auth`, `diff`, `init`, `repair`, `upgrade` (smaller surfaces first).
4. Migrate `pull/*` drivers.
5. Migrate `push/*` drivers.
6. Migrate `deploy/*` (apply, create, realign, run, hook_secrets).
7. Migrate `sync/execute.rs` and `sync/mod.rs`.
8. Migrate `sync/watch.rs` (drop `[HH:MM:SS]` brackets, drop `now_hhmmss` private copy).
9. Migrate residual `eprintln!`/`println!` in `main.rs`, `env_picker.rs`, `resolve.rs`.
10. Delete `src/progress/` and any now-unused types.
11. Remove `indicatif` from `Cargo.toml` if no consumer remains.
12. Update / retire affected specs and integration-test fixtures.
