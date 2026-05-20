# Sync grid visualization — design

> Status: draft (experiment).
> Replaces the per-line event log in `rdc sync <env>` and `rdc sync --watch <env>` with an inline, kind-grouped grid of colored squares. One square per `(kind, slug)`. Color encodes a hybrid freshness clock plus state stamps from the existing classifier.

## 1. Goal

The current watch UX is a stream of timestamped lines. It scrolls, it's noisy, and it answers the question *"what just changed?"* much better than *"how is everything doing right now?"*. This spec describes a glance-able dashboard that answers the second question.

The result is a fixed region of the terminal that redraws in place during a sync cycle, persists as the last frame of output after the cycle exits, and falls back cleanly to today's event-log format on non-TTY stderr.

## 2. Resolved decisions (anchor)

Decisions locked during brainstorming, in the order they were made:

1. **Hybrid color semantics.** Default behavior is a freshness clock — a successful classifier verdict resets a square to bright green; time passing without a touch ages it green → yellow → orange → red. State changes from the classifier *stamp* the square with a meaningful color regardless of clock age: local edits stamp red, remote-only changes stamp orange, conflicts stamp red with a yellow outline.
2. **Scope: both commands.** `rdc sync <env>` (one-shot) and `rdc sync --watch <env>` (foreground continuous) both use the grid. One-shot opens with the current state from the lockfile, animates through the cycle, and the final frame stays in scrollback on exit.
3. **Default, not opt-in.** No flag. The grid replaces the event log entirely on a TTY. Non-TTY auto-falls back to the existing format so CI is undisturbed.
4. **Inline in-place redraw + grid-keeps-its-place for prompts.** No alt-screen takeover. Conflict prompts appear *below* the grid as additional lines while the grid keeps redrawing above.
5. **Layout: kind-grouped rows.** One row per kind (`workspaces`, `queues`, `schemas`, ...) with a left-anchored 18-char label that includes the per-kind count. Rows wrap into continuation lines when a kind has more squares than fit in one terminal width.
6. **Universe of squares = union(lockfile entries, local-only, remote-only).** Read-only kinds (`organization`, `workflows`, `workflow_steps`) get squares but only ever paint Clean / RemoteEdit / RemoteCreate states.
7. **SyncClass → square state mapping.** Eleven classifier verdicts fold into the hybrid model as a fixed table (Section 5.2).
8. **Decay schedule.** Bright green for 0–15s, green for 15s–1m, yellow for 1m–5m, orange for 5m–15m, red beyond 15m. (Half of the initially proposed bands.)
9. **Reset rule.** Any successful round-trip touching the resource resets its clock to `now()` — *including* a "verified clean" cycle that fetched the remote, hashed it, and confirmed equality with the lockfile base. The clock answers "when did rdc last see this resource in any state?"
10. **Identification surface = a live footer.** Below the grid, every currently non-clean resource is named by full path with its severity tag. The footer collapses to a single `all clean (N)` line when nothing is amiss, but the kind rows themselves are *always* visible — they never collapse.
11. **In-flight pulse.** Squares being actively touched (GET / PATCH / POST / DELETE in flight) pulse at ~4 Hz with 30 % brightness reduction; the header carries a spinner + current operation string.
12. **Non-TTY fallback to today's event log format.** No new plain-text schema invented.
13. **One-shot exit leaves the final frame in scrollback.** Watch Ctrl-C does the same plus a `stopped after N cycles, Mm runtime` summary.
14. **Freshness clock keeps logically advancing during interactive prompts.** When the prompt returns and the grid redraws, squares snap to whatever color their elapsed age now warrants. No special pause state.
15. **Cold-start grid is empty.** The grid is populated by the first `ingest_classification` at the end of phase 1.
16. **Implementation approach: extend `indicatif::MultiProgress`.** Reuse the proven infrastructure already used by `src/progress.rs`. No new heavy dependency (no `ratatui`, no hand-rolled crossterm grid).

## 3. Architecture

A new module tree replaces today's single `src/progress.rs` file. The existing public API (`ProgressLog`, `Phase`, `Spinner`) is preserved; it just moves into a submodule and is used by callers that aren't sync.

```
src/progress/
├── mod.rs       — SyncRenderer trait + dispatcher
├── log.rs       — existing ProgressLog impl (moved verbatim)
└── grid.rs      — new GridRenderer impl
```

Callers of the event log today: `deploy`, `pull`, `push`, `diff`, `repair`. None of them change. They keep instantiating `ProgressLog::start(...)` from `progress::log`.

The two sync entry points construct the renderer through a dispatcher:

```rust
let renderer: Arc<dyn SyncRenderer> = make_sync_renderer(&title, env, is_watch);
```

`make_sync_renderer` returns a `GridRenderer` when stderr is a TTY and a `LogRenderer` (thin wrapper around the existing `ProgressLog`) otherwise. TTY presence is the only signal — there is no `--ui` flag, no env var override.

### 3.1 The `SyncRenderer` trait

```rust
pub trait SyncRenderer: Send + Sync {
    fn phase(&self, label: &str);

    fn resource_started(&self, kind: &str, slug: &str, op: ResourceOp);
    fn resource_finished(&self, kind: &str, slug: &str, outcome: ResourceOutcome);

    fn ingest_classification(&self, items: &[ClassifiedItem]);

    fn banner(&self, severity: Severity, msg: &str);

    fn with_prompt<R>(&self, f: impl FnOnce() -> R) -> R;

    fn finish_ok(&self, summary: &str);
    fn finish_err(&self, msg: &str);
}

pub enum ResourceOp { Get, Patch, Post, Delete }
pub enum ResourceOutcome { Ok, Skipped, Failed(String), ConflictPending }
pub enum Severity { Info, Warn, Error }
```

`LogRenderer` implements `resource_started` / `resource_finished` / `ingest_classification` as no-ops; the per-resource events are a grid-only concern. The result is that the refactor is **purely additive** for the event-log path — every existing event-log output line is preserved byte-for-byte.

### 3.2 Plumbing change

`PullCtx` and the per-kind push/pull driver signatures change from `&Arc<ProgressLog>` to `&Arc<dyn SyncRenderer>`. The change is mechanical and covers ~25 files (Section 6.3). It's larger in line count than in semantic surface.

## 4. State model

### 4.1 `GridState`

```rust
pub(crate) struct GridState {
    entries: BTreeMap<(String, String), Entry>,
    order: BTreeMap<String, Vec<String>>,
    env: String,
    cycle: u64,
    started_at: Instant,
    poll_interval: Option<Duration>,
    current_op: String,
    banners: VecDeque<Banner>,
}

struct Entry {
    last_verified_at: Instant,
    class: SyncClass,
    in_flight: Option<ResourceOp>,
}
```

- `entries` is keyed by `(kind, slug)`. The universe is the union (lockfile entries ∪ local-only ∪ remote-only). `BothDeleted` is removed at ingest time.
- `order` records the per-kind canonical render order — slug-alphabetical, fixed at first observation so rows don't shuffle. New `(kind, slug)` keys append at the right by alphabetical insertion.
- `current_op` is the most recent `phase()` label, displayed in the header.
- `banners` is a FIFO of transient error / info messages with a 5-second display window each.

### 4.2 Ingest / eviction rules

- **Create**: on the first `ingest_classification` that contains `(kind, slug)`. `last_verified_at = now()`. `class` is set from the verdict. `in_flight = None`.
- **Update**: every subsequent `ingest_classification`. `class` is overwritten. `last_verified_at` advances to `now()` *iff* the resource was observed by this cycle in any class (so a failed kind-list does not reset clocks for that kind).
- **Evict**: after two consecutive cycles where the resource doesn't appear in the classification, the entry is removed. (Defensive against a single transient kind-list failure.)

`BothDeleted` is special-cased: it never creates an entry, and if an existing entry transitions to `BothDeleted` it's removed immediately.

### 4.3 Why `Instant`, not wall clock

The freshness clock is display-only and must be monotonic. NTP steps, suspend/resume, and clock drift all happen in wall time but not in `Instant`. Wall-clock timestamps appear only in the header (`uptime 14m`) and banner labels (`[14:09:21] !`).

## 5. Color computation

### 5.1 `color_for(entry, now) -> Color`

Pure function. Stamps short-circuit the clock:

```rust
fn color_for(e: &Entry, now: Instant) -> Color {
    match e.class {
        SyncClass::LocalEdit
        | SyncClass::LocalCreate
        | SyncClass::LocalDelete            => return Color::EditRed,
        SyncClass::RemoteEdit
        | SyncClass::RemoteCreate
        | SyncClass::RemoteDelete           => return Color::PendingOrange,
        SyncClass::BothDiverged
        | SyncClass::LocalEditRemoteDelete
        | SyncClass::LocalDeleteRemoteEdit  => return Color::ConflictOutlined,
        SyncClass::BothDeleted              => unreachable!(),
        SyncClass::Clean                    => { /* fall through */ }
    }

    let age = now.saturating_duration_since(e.last_verified_at);
    match age.as_secs() {
        0..=15      => Color::FreshGreen,
        16..=60     => Color::Green,
        61..=300    => Color::Yellow,
        301..=900   => Color::Orange,
        _           => Color::StaleRed,
    }
}
```

### 5.2 SyncClass → square state mapping

| SyncClass | Square state |
|---|---|
| `Clean` | freshness clock (5-band) |
| `LocalEdit` | red stamp (edit pending push) |
| `LocalCreate` | red stamp (new, pending POST) |
| `LocalDelete` | red stamp + striped overlay (tombstone, pending DELETE) |
| `RemoteEdit` | orange stamp (pending pull) |
| `RemoteCreate` | orange stamp + outline (new on remote, pending pull) |
| `RemoteDelete` | orange stamp + striped overlay (tombstone on remote, pending reconcile) |
| `BothDiverged` | red + yellow outline (conflict — needs prompt) |
| `LocalEditRemoteDelete` | red + yellow outline (conflict) |
| `LocalDeleteRemoteEdit` | red + yellow outline (conflict) |
| `BothDeleted` | no square (silently converged) |

### 5.3 In-flight pulse

`entry.in_flight = Some(op)` triggers a 30 %-brightness reduction toggle at the 250 ms tick. Applied as an overlay at render time — `color_for` itself doesn't observe `in_flight`.

### 5.4 Palette (24-bit truecolor)

| Color | RGB |
|---|---|
| `FreshGreen` | `#1f6e3e` |
| `Green` | `#2a8a4b` |
| `Yellow` | `#c79a2b` |
| `Orange` | `#d8612e` |
| `StaleRed` | `#a52a2a` |
| `PendingOrange` | `#e89622` |
| `EditRed` | `#ff3b30` |
| `Conflict` | `#c93030` bg + `#ffd166` outline char |

### 5.5 Color-depth detection and fallback

```
NO_COLOR set            → ColorDepth::None  (ASCII: "· " / "o " / "x ")
COLORTERM = truecolor   → ColorDepth::TrueColor
TERM contains 256color  → ColorDepth::Color256 (precomputed cube indices)
otherwise               → ColorDepth::Color16  (3-band collapse: green/yellow/red)
```

`Color16` loses freshness granularity but stays readable. `None` is a tabular ASCII view — usable in places where color is intentionally disabled.

## 6. Rendering

### 6.1 `MultiProgress` layout

Bars are pre-allocated at `GridRenderer::new` and never reshuffled. `set_message` per tick changes content only.

```
bar  0:  HEADER          spinner + env + counts + current op + uptime
bar  1:  KIND_ROW[0]     "workspaces (4)    ▇▇ ▇▇ ▇▇ ▇▇"
bar  2:  KIND_ROW[1]     "queues (24)       ..."
...
bar  K:  KIND_ROW[K-1]   one per kind in canonical order
bar  K+1: SEPARATOR       blank
bar  K+2: BANNER[0]
bar  K+3: BANNER[1]
bar  K+4: FOOTER_HDR      "current state:" or "all clean (N)"
bar  K+5..K+16: FOOTER[0..11]   non-clean resources, severity-sorted
bar  K+17: FOOTER_MORE    "+ N more (run `rdc sync --dry-run` for full list)" or blank
```

Continuation rows for wrapping kinds: each kind can have up to `max_continuation` extra bars allocated. The max is determined after the first ingest from the universe size — we know the maximum number of squares each kind will ever hold, and therefore the maximum number of continuation rows for any reasonable terminal width.

### 6.2 Glyph and ANSI

Each square is **two spaces with an ANSI background color**, followed by a one-space gap:

```
ESC[48;2;<R>;<G>;<B>m  ESC[0m   <gap>
```

Square width is 2 columns; total cell footprint (square + gap) is 3 columns. With an 18-char label prefix and an 80-col terminal, that's `(80 - 18 - 1) / 3 = 20` squares per row before wrapping.

The conflict outline is a single bright glyph in the gap position adjacent to the cell (`▏▕` left/right brackets — final choice deferred to implementation, see Section 9.2). It reads as "this one is special" without breaking column alignment.

### 6.3 Header content

```
{spinner} rdc sync --watch <env> · 286 clean · 2 pending · 1 conflict · pulling schemas (12/24) · uptime 14m
```

- `{spinner}`: indicatif's built-in tick string (`|/-\`), reused from existing `progress::log`.
- Counts are clean / pending (anything orange-stamped) / conflict (anything red-outlined).
- Current op is the most recent `phase()` label; `idle` between cycles.
- `uptime` only shows in watch mode.

### 6.4 Footer content

```
current state:
conflict  schemas/cost-invoices
edit      hooks/finance-totals
pending   queues/manual-review
```

Sorted by severity descending (conflict > edit > pending), then `kind/slug` alphabetically. Cap at 12 lines; overflow into a single `+ N more` summary. When everything is Clean, the FOOTER_HDR shows `all clean (N)` and every footer slot is blank — kind rows stay visible per decision 10.

### 6.5 Cadence and width

- `MultiProgress::set_draw_target(ProgressDrawTarget::stderr_with_hz(8))`: 8 Hz redraw.
- `enable_steady_tick(Duration::from_millis(250))` per bar: drives the in-flight pulse.
- `crossterm::terminal::size()` queried at startup and on SIGWINCH (with a 100 ms debounce). Width below 21 columns degrades to a count-only mode.

## 7. Integration

### 7.1 `cli::sync::run_cycle`

`run_cycle` is called both directly by `cli::sync::run` (one-shot) and by `watch::event_loop` (each cycle). To avoid constructing a fresh renderer per watch cycle — which would discard freshness clocks across ticks — its signature grows an optional pre-constructed renderer:

```rust
pub(crate) async fn run_cycle(
    env: &str,
    interactive: bool,
    dry_run: bool,
    diff: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    renderer: Option<Arc<dyn SyncRenderer>>,
) -> Result<CycleOutcome> {
    let renderer = renderer.unwrap_or_else(|| make_sync_renderer(&title, env, is_watch=false));

    renderer.phase("listing remote");
let catalog = list_remote(...).await?;

renderer.phase("scanning local");
let (_, changes, tombstones) = push::scan::scan(...)?;

renderer.phase("classifying");
let classified = from_catalog_scan_lockfile(...);
renderer.ingest_classification(&classified);

if dry_run { renderer.finish_ok(...); return Ok(...); }

renderer.phase("executing");
let outcome = execute::run(..., &renderer).await?;

let classified_after = from_catalog_scan_lockfile(...);
renderer.ingest_classification(&classified_after);

renderer.finish_ok(...);
```

Two `ingest_classification` calls per cycle: pre-execute (paint current state from lockfile + first-listing) and post-execute (refresh squares that flipped Clean). The second call doesn't re-list remote.

### 7.2 `cli::sync::watch::event_loop`

The renderer is constructed once in `run_watch` above the loop, passed into every `run_cycle` call via the `Option<Arc<dyn SyncRenderer>>` argument from 7.1, and persists across cycles. `GridState` keeps accumulating freshness timestamps for the life of the watch. The current `print_cycle_summary` is deleted — the grid is the summary. Verbose / counters still flow through `CycleOutcome` for callers that need them.

The one-shot path passes `None` and `run_cycle` constructs a fresh renderer that's torn down on cycle exit.

### 7.3 Per-resource event emission

Each per-kind push/pull driver iterates over a known object set. Each iteration grows two calls:

```rust
renderer.resource_started("hooks", slug, ResourceOp::Patch);
let result = patch_hook(...).await;
match &result {
    Ok(_)  => renderer.resource_finished("hooks", slug, ResourceOutcome::Ok),
    Err(e) => renderer.resource_finished("hooks", slug, ResourceOutcome::Failed(e.to_string())),
}
result?;
```

Affected push files: `src/cli/push/{hooks,rules,labels,workspaces,queues,schemas,inboxes,email_templates,engines,engine_fields,deletes}.rs` — 11 files.

Affected pull files: `src/cli/pull/{hooks,rules,labels,workspaces,queues,schemas,inboxes,email_templates,engines,engine_fields,mdh,organization,workflows,workflow_steps}.rs` — 14 files.

Each touch is roughly 2 lines of additive code. Total surface is large in line count, small in semantic risk.

### 7.4 Prompt coexistence

```rust
fn with_prompt<R>(&self, f: impl FnOnce() -> R) -> R {
    let suspended = self.mp.set_draw_target(ProgressDrawTarget::hidden());
    let result = f();
    self.mp.set_draw_target(ProgressDrawTarget::stderr_with_hz(8));
    self.force_redraw();
    result
}
```

The existing inline resolvers (`prompt_resolve`, `prompt_remote_delete`, the destructive-delete gate in `push::deletes`) are wrapped in `with_prompt`. The grid suspends redraw for the duration; prompt text appears below the (now-static) grid frame; on resolution the grid redraws above, and the prompt text scrolls into terminal history naturally.

The freshness clock keeps logically advancing because `Instant::now()` keeps moving. When the prompt returns and the grid redraws, squares snap to whatever color their elapsed age warrants. No special pause state is required on `GridState`.

### 7.5 Auth refresh

```rust
Err(e) if anyhow_has_status(&e, 401) => {
    renderer.banner(Severity::Warn, "auth expired — refreshing token");
    renderer.with_prompt(|| refresh_token_interactively(env)).await?;
}
```

Banner appears in `BANNER[0]` for 5 seconds; the refresh prompt itself routes through `with_prompt`.

### 7.6 Exit semantics

On `finish_ok` / `finish_err`:

1. `set_draw_target(ProgressDrawTarget::hidden())` to stop redraw.
2. `mp.println` each bar's final content as a permanent line.
3. Drop `MultiProgress`.

This commits the final frame into scrollback. Indicatif's normal "clear on drop" is bypassed by the explicit println-then-hide pattern.

Watch Ctrl-C: shutdown handler calls `renderer.finish_ok("stopped after N cycles, Mm runtime")`. Same commit-and-drop sequence.

## 8. Error handling and edge cases

| Case | Behavior |
|---|---|
| `crossterm::terminal::size()` fails | Fall back to 80 columns. |
| Terminal width < 21 | Switch to count-only mode; log one warning banner. |
| SIGWINCH burst | 100 ms debounce on resize events. |
| Indicatif panic / `set_message` failure | Wrapped in `let _ = ...` with a debug-assert; production silently no-ops the affected bar for one tick. |
| ANSI escape mid-write process death | Worst case: one corrupt line; subsequent runs unaffected. |
| Kind disappears mid-watch (e.g., MDH dropped from cluster) | Two-cycle no-show evicts entries; kind row shrinks to zero squares but stays visible per decision 10. |
| Re-classification within one cycle (conflict resolved → LocalEdit → Clean) | Each state may be visible briefly at 8 Hz. Acceptable. |
| Slug rename after `rdc repair --rename-slugs` | New `(kind, slug)` keys; old entries hit two-cycle no-show. Row reshuffles within a kind — acceptable, rare. |
| Resumed long-paused watch | First paint shows all-red-stale; first cycle resets all clocks to fresh green. Two-frame transition. |
| `--dry-run` | Renderer constructed; `execute::run` skipped; renderer commits one frame; binary exits. No special path. |

## 9. Open implementation defaults

Three minor decisions deferred to implementation time. The defaults below take effect unless reviewers flag them before coding starts.

### 9.1 Wrap order within a kind

Slug alphabetical, wrap left-to-right top-to-bottom. Stable across redraws.

### 9.2 Conflict outline glyph

Provisional choice: `▏` / `▕` (left half-block / right half-block). Final selection during implementation based on rendering in macOS Terminal.app and iTerm2. Fallback if these have width quirks: `[` / `]`.

### 9.3 Banner deduplication

Identical-text banners arriving within 10 seconds are folded into one with a `(×N)` suffix.

## 10. Testing strategy

| Layer | Test shape |
|---|---|
| `color_for` | Property tests on band edges (15s, 60s, 300s, 900s) and on each stamp short-circuit. |
| `GridState` ingest | Sequence-of-snapshots fixtures; assert entries grow/shrink correctly; two-cycle no-show evicts; clock advance rule. |
| Layout / wrapping | Snapshot tests at widths 40 / 80 / 120 / 200. |
| ANSI emission | Per `ColorDepth`: assert the exact escape string emitted for each palette color. |
| `RecordingRenderer` fake | Used by `run_cycle` / `event_loop` integration tests to assert event order without rendering. The existing watch tests adopt this so they keep working in CI without a real terminal. |
| Non-TTY dispatch | `make_sync_renderer(stderr_is_tty=false)` returns `LogRenderer`; output matches today's event-log format byte-for-byte. |
| Prompt coexistence | Drive a `BothDiverged` conflict; assert `set_draw_target(Hidden)` precedes stdin read and `stderr_with_hz(8)` follows it. |
| Color-depth detection | Table tests across `(NO_COLOR, COLORTERM, TERM)` combinations. |

### 10.1 Behavior parity gate

Before merging, one PR-checklist item runs `cargo test` with stderr redirected to a file (forces `LogRenderer`). Output of the relevant `rdc sync` / `rdc sync --watch` integration tests must match today's recorded fixtures byte-for-byte. This is the safety net: even if the grid renderer has bugs, CI sees today's logs unchanged.

### 10.2 What is explicitly not tested

- Visual fidelity in real terminals — manual PR-checklist smoke test against a fixture project.
- Indicatif internals.
- Exact frame timing of the 250 ms pulse.

## 11. Non-goals

Explicit, so a future contributor doesn't need to ask:

- No `--ui` flag, no env-var override. TTY presence is the sole switch.
- No configurable thresholds. The 15s / 1m / 5m / 15m bands and the 8 Hz / 250 ms cadences are hard-coded.
- No mouse interaction. No alt-screen takeover. No keybindings beyond Ctrl-C.
- No persistent state across runs. Each `rdc sync` rebuilds from empty in the first 1–2 seconds.
- No theme support / light-mode detection. 24-bit palette assumes a dark terminal.
- No grid for `rdc deploy`. Deploy is plan-confirm-apply; its event log works.
- No replacement for `--diff` rendering. Diffs stay as unified diffs.

## 12. Out-of-scope, deferred to follow-up if user demand emerges

- Per-kind row hiding (`RDC_GRID_HIDE_KINDS=mdh,workflows`).
- Inline transition timestamp in the footer (`edit 14:09:03 hooks/finance-totals`).
- Search / filter / "only red squares" view.
- Cross-env diff visualization.
- Light-theme palette.
