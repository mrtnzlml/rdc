# Sync Watch Mode — Design

**Status:** Spec, awaiting review
**Date:** 2026-05-14
**Scope:** Add `rdc sync --watch <env>` — a foreground watch mode that keeps a single env continuously reconciled. Outbound: file-system events on the local snapshot trigger a sync cycle. Inbound: a configurable poll (default 60s) catches remote drift. Conflicts and destructive deletes prompt inline in the watch terminal. Ctrl-C stops it.

## Goal

Close the "in sync all the time, non-intrusively" gap left open by one-shot `rdc sync`. The unified-sync spec deferred this as out-of-scope; this spec is its follow-on.

The user starts `rdc sync --watch test` in a terminal tab, leaves it running, and edits files in another window or in the Rossum UI. The daemon converges both sides automatically. When human input is required (conflict or destructive delete), the prompt appears in the watch terminal.

## Non-goals

- **True backgrounded daemon (fork/detach).** The process stays attached to the terminal. No PID file lifecycle, no separate `daemon start` / `daemon stop` subcommands, no log file. Ctrl-C is the only stop mechanism.
- **A sidecar IPC / resolve command.** The watch terminal IS the resolve UI.
- **Multi-env watch in one invocation.** One env per `rdc sync --watch`. Users wanting to watch multiple envs open multiple terminal tabs.
- **Watching `rdc.toml` or `secrets/`.** Config changes (api_base, org_id, tokens) require daemon restart. The watcher only follows `envs/<env>/` and the env's `overlay.toml`.
- **Custom debouncing strategies / tunable file-event coalesce windows.** One sensible default (500 ms).
- **Plan-and-confirm for every cycle.** Watch mode auto-applies non-destructive changes silently. Conflicts and destructive deletes still prompt — those are the only points that require human judgment.
- **OS-native notifications, system tray, sound, etc.** Pure terminal output.
- **Pause/resume signaling.** Concurrency is handled via short advisory locks per write window, not signal-based pause/resume.

## Background

- Unified sync (`rdc sync <env>`) landed earlier this release — see `docs/superpowers/specs/2026-05-14-unified-sync-design.md` and `docs/superpowers/plans/2026-05-14-unified-sync.md`. The motivation Martin gave at the start of brainstorming was: *"In ideal case, the local and remote environments are in sync all the time (but non-intrusively)."* That spec scoped the daemon as deferred; this spec delivers it.
- rdc today has **no daemon infrastructure**: no file watcher, no advisory locking, no IPC, no background process management. The conflict resolver reads `std::io::stdin()` directly (`src/cli/resolve.rs:108`) and emits prompts to whatever `Write` the caller passes — currently always the TTY.
- The lockfile (`.rdc/state/<env>.lock.json`) is written atomically via temp-rename (per the README's "Atomic on disk" principle). That handles crash safety but not write-write races between two concurrent rdc commands.
- All existing rdc commands are request-response (start, do one thing, exit). Watch mode is the first long-lived command.

## Design

### CLI surface

```
rdc sync <env> --watch [--poll-interval <duration>] [--no-poll] [--allow-deletes] [--no-push] [--no-pull] [-v]
```

The existing one-shot `rdc sync <env>` is unchanged.

- `--watch` enters the watch loop after an initial reconcile.
- `--poll-interval <duration>` (default `60s`): cadence for the remote drift poll. Accepts human-readable durations (`30s`, `2m`, `5m`).
- `--no-poll` disables inbound polling entirely; only outbound (file-event-driven) sync happens.
- `--allow-deletes`: same meaning as one-shot — permit `local-tombstone → remote DELETE` without per-cycle prompts.
- `--no-push` / `--no-pull`: same meaning as one-shot. Useful for "audit-watch" (CI-style drift monitoring) and "deploy-watch" (read-only-local).
- `-v` / `--verbose`: print every cycle, even no-op cycles. Default is quiet (print only cycles that did something).

**Flag conflicts** (rejected at clap parse time):

- `--watch --dry-run` — incoherent. Watch is interactive over time; dry-run is a one-shot preview. Error: `--dry-run cannot be combined with --watch.`
- `--watch --diff` — only valid with `--dry-run`, so transitively rejected.
- `--watch --yes` — *accepted*. Semantics match one-shot `--yes`: non-TTY fallback for conflicts (skip-with-shadow). Destructive deletes still gated by `--allow-deletes`. Useful for unattended-CI watch scenarios.
- `--watch --no-push --no-pull` — same error as the one-shot mutex.

### Trigger model

Two independent triggers feed a single cycle queue:

1. **File-system events** on `envs/<env>/` (recursive) and `envs/<env>/overlay.toml`. Powered by the `notify` crate (`inotify` on Linux, `kqueue`/`FSEvents` on macOS, `ReadDirectoryChangesW` on Windows).
   - **Excluded paths**: `.rdc/` (daemon-managed), `*.<env>` shadow files, `*.<env>-deleted` markers (avoids feedback loops when sync writes shadows).
   - **Debounce**: 500 ms after the last event before queuing a cycle. Editors that write multiple events per save (tmpfile + rename + chmod) coalesce to one cycle.

2. **Poll timer**: fires every `<poll-interval>` (default 60s). Disabled by `--no-poll`. Each fire enqueues a cycle.

A single **cycle worker** drains the queue: at most one cycle runs at a time. If new events arrive during a cycle, they enqueue and run after the current cycle completes (idempotent re-runs are cheap).

### Initial reconcile

On `rdc sync --watch test` start:

1. **Acquire** the env's advisory lock (see Concurrency).
2. **Run one full sync cycle** (identical to `rdc sync test` one-shot): list remote, scan local, classify, plan, confirm if needed, execute. This brings the env to a known state before the watch loop begins.
3. **Release the lock**, start the file watcher, start the poll timer.
4. Print: `watching envs/test/ ...` and `polling test every 60s ...` (or `polling disabled` if `--no-poll`).

If the initial reconcile raises a conflict or destructive-delete prompt, it surfaces in the watch terminal exactly as a one-shot would. After resolution, the daemon proceeds to step 3.

### Watch cycle

Each cycle is identical to a one-shot sync EXCEPT:

- **No plan-and-confirm prompt for non-destructive cycles.** If the classification produces zero conflicts and zero destructive items, the cycle executes silently and prints a one-line summary:
  ```
  [14:02:17] queues/cost-invoices, schemas/cost-invoices         → push (0.6s)
  [14:03:02] (idle)                                              # only with -v
  [14:05:41] hooks/validator-invoices                            ← pull (0.4s)
  ```
- **Conflicts still prompt.** The resolver UI is unchanged; it appears inline.
- **Destructive deletes still prompt** unless `--allow-deletes`.
- **`[a]bort` semantics shift.** In a one-shot, abort exits the command. In watch, abort skips THIS cycle (don't save lockfile), prints `[14:02] aborted by user (lockfile unchanged)`, and continues watching. The conflict will re-surface on the next cycle. Ctrl-C is the only way to stop the daemon.
- **Errors don't terminate.** A network error, 401, or parse error logs and continues. Specifically:
  - **Network 5xx / timeout**: log `[14:02] poll failed: connection timed out, retrying in 60s`, continue the next cycle as scheduled.
  - **401 Unauthorized**: pause cycle, prompt inline for new token (we have TTY); save token via existing `auth::refresh_token_interactively`, retry the cycle once. Same UX as `with_401_retry` already provides for one-shot commands.
  - **Local file parse error** (malformed JSON the user just saved): log `[14:02] envs/test/labels/audit-hold.json: invalid JSON at line 3, skipping this cycle`. The user fixes the file; the next save event triggers another cycle.

### Conflict / destructive-delete prompting

Uses the existing `cli::resolve::prompt_resolve_with_color` and `prompt_remote_delete_with_color`. No new prompt code needed — the watch loop calls them with `std::io::stdin().lock()` and `std::io::stderr().lock()`, identical to one-shot.

**One UX adjustment**: when the watch loop is waiting on user input, file events and poll timers continue to enqueue cycles, but the cycle worker is blocked on the prompt. This is the desired behavior — don't fire another sync while the user is mid-resolve. After the user submits a resolution, the worker completes the cycle and drains any queued events as fast as it can.

### Concurrency: advisory locks

Add a sibling lock file: `.rdc/state/<env>.lock`. Distinct from the existing `.rdc/state/<env>.lock.json` (the lockfile content). The new file is empty; it exists only for `fcntl(F_SETLKW)` / `LockFileEx` to acquire/release.

Use the `fs4` crate (cross-platform advisory locks; a maintained fork of `fs2`). Add to `Cargo.toml`.

**Lock acquisition points:**

- **Watch cycle**: acquire exclusive lock at the start of execute phase; release after lockfile save. Held for the duration of one cycle (typically <2s).
- **One-shot `rdc sync`**: same pattern — acquire exclusive lock at execute, release after save.
- **`rdc deploy <src> <tgt>`**: acquire exclusive lock on `<tgt>.lock` for its write phase. Source env is read-only and uses atomic-rename consistency.
- **`rdc repair`** and **`rdc pull`-equivalent flows**: same pattern.
- **Read-only commands** (`status`, `diff`): no locking. Reads tolerate atomic-rename.

**Lock contention behavior:**

- Try to acquire with a 30-second timeout via `fs4::FileExt::try_lock_exclusive` in a loop with short sleeps. If still blocked after 30s, fail with: `error: another rdc process is writing 'test' (waited 30s). Check 'rdc sync --watch' or pending operations.`
- Print a single hint while waiting: `(waiting for lock on 'test'... NNs)`.

### Feedback-loop prevention

When the watch cycle writes local files (pull-side outcomes, conflict `[r]` outcomes, resolver `.<env>` shadows), the file watcher would otherwise re-trigger. Two-part suppression:

1. **Watcher pauses during a cycle**: cycle worker calls `watcher.unwatch(...)` on entry, `watcher.watch(...)` on exit. Cheap; the `notify` crate supports it.
2. **Post-cycle drain**: any events that arrived between unwatch and the next watch start are discarded by design (the cycle just wrote that state, so any "change" event is the cycle's own write). The post-cycle re-scan is driven by classification, not by stale events.

This means: a save that arrives mid-cycle gets dropped on the floor, AND the user's editor save signal that triggered the cycle has already been observed. Net effect: no missed user edits, no feedback loops.

### Output format

Default (quiet):

```
$ rdc sync --watch test
watching envs/test/ (recursive) ...
polling test every 60s ...

[14:02:17] queues/cost-invoices                                → push (0.6s)
[14:03:42] hooks/validator-invoices, hooks/validator-totals    → push (1.1s)
[14:05:41] labels/audit-hold (new)                             ← pull (0.4s)
[14:09:03] hooks/finance-totals — conflict
  local has changes:
    - threshold: 0.85
    + threshold: 0.95
  test has changes:
    - threshold: 0.85
    + threshold: 0.80
  [k] keep local  [r] use test  [e] edit  [s] skip  [a] abort > k
[14:09:21] hooks/finance-totals                                → push (0.4s) [resolved]
```

Verbose (`-v`):

```
[14:02:17] queues/cost-invoices                                → push (0.6s)
[14:02:42] (idle)
[14:03:42] (idle)
[14:04:42] (idle)
[14:05:41] labels/audit-hold (new)                             ← pull (0.4s)
```

The `(idle)` lines fire on every poll-timer tick when classification produces zero items.

### Error UX

- Network errors during poll: stderr `[14:02] poll: timed out (1/5 retries)`. Backoff: 1s, 2s, 4s, 8s, 16s. Same shape as the one-shot retry logic.
- Lock contention: `(waiting for lock on 'test'... 2s)`, then proceeds when the holder releases.
- Token expiry: inline `auth::refresh_token_interactively` prompt; daemon resumes after.
- Daemon's own crash (panic): the user sees a Rust panic and the process exits. Locks are released by the OS on process exit. The user restarts manually.

### Code shape

**New module**: `src/cli/sync/watch.rs`. Public surface:

```rust
pub async fn run_watch(
    env: &str,
    interactive: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    poll_interval: Option<std::time::Duration>,  // None = --no-poll
    verbose: bool,
) -> anyhow::Result<()>
```

Composition:

1. Acquire lock; run one `sync::run_cycle` (extracted from current `sync::run`); release lock.
2. Spawn `notify` watcher → channel.
3. Spawn `tokio::time::interval` poller (if poll-interval is Some) → same channel.
4. Spawn ctrl-c handler → same channel.
5. Main loop: `select!` on the channel; on each event, acquire lock, run one `sync::run_cycle`, release lock; on ctrl-c, break.

**Refactors** (minimal):

- Extract the "one sync cycle" from `cli::sync::mod.rs::run` into a `sync::run_cycle(...)` helper. The existing `run` becomes: lock + run_cycle + release, with the same plan-confirm-execute flow it has today (so one-shot UX is preserved). Watch calls `run_cycle` directly with `confirm=false` for non-destructive cycles.
- Add a `Config` struct or extend `sync::run` parameters with `confirm_non_destructive: bool` so watch mode can suppress the plan prompt while preserving conflict and destructive prompts. The existing `interactive` flag already drives those.
- Wrap one-shot `sync::run` to take the lock at the start of its execute phase and release at the end.

**New dependencies** (added to `Cargo.toml`):

- `notify` (latest 6.x) — file watcher.
- `fs4` (latest) — cross-platform advisory locking. Maintained fork of `fs2`.

Both have small dependency trees; no significant compile-time cost.

**New CLI variant** in `src/cli/mod.rs::Command`:

The existing `Sync { ... }` variant gains `--watch`, `--poll-interval`, `--no-poll`, `-v`. Routing in `run` branches: if `watch == true`, dispatch to `cli::sync::watch::run_watch`; else to `cli::sync::run`.

### Testing

Watch mode is the hardest piece in rdc to test end-to-end. Strategy:

**Unit tests** (in `src/cli/sync/watch.rs::tests`):

- **Debounce coalesces multiple events**: feed N synthetic events within 100ms via the event channel; assert exactly one cycle runs.
- **Poll timer drives cycles independent of events**: with `--no-poll`, time advance produces zero cycles; with 60s interval, time advance of 70s produces one cycle.
- **Lock contention waits then proceeds**: spawn a second task holding the lock for 200ms; assert the first task completes after ~200ms.
- **Conflict during cycle re-prompts on next cycle if aborted**: synthetic `[a]bort` resolution; second cycle still surfaces the same conflict.
- **`[a]bort` skips the cycle, doesn't save lockfile**: verify lockfile bytes are byte-identical after an aborted cycle.

For deterministic time, use `tokio::time::pause()` + `tokio::time::advance()` (already supported by tokio's test utilities).

For synthetic file events, inject directly into the cycle channel — don't try to drive the OS file watcher in tests (flaky, platform-specific).

**Integration tests** (in `tests/cli_sync.rs`):

- **`sync_watch_initial_reconcile_then_idle`**: start `run_watch` with `--no-poll`, no local edits, empty wiremock. Run for 100ms. Assert: exactly one cycle ran (the initial reconcile), no further cycles, clean exit on synthetic ctrl-c.
- **`sync_watch_local_edit_triggers_push`**: start `run_watch`, write to a local label file mid-run, wait 600ms (past debounce), assert PATCH was hit once on the mock.
- **`sync_watch_poll_catches_remote_drift`**: start `run_watch` with `--poll-interval=100ms`, change the mock's label response after start, assert local file is rewritten within 200ms.
- **`sync_watch_lock_blocks_one_shot`**: start `run_watch` holding the lock for one cycle, run `sync::run` from another task, assert it waits then completes.
- **`sync_watch_ctrl_c_clean_shutdown`**: signal SIGINT mid-cycle, assert lock is released and lockfile is unchanged.

The watch integration tests will need a small in-process test harness for sending ctrl-c-equivalent signals (use a shutdown channel rather than real signals — easier to test).

**Out of test scope**: real `notify`-backed file events (the unit/integration tests use channel injection); real OS signal delivery; cross-platform behavior differences (CI verifies Linux; macOS-on-Apple-Silicon is the developer's daily-driver; Windows is a known gap, see Open Questions).

### Compatibility

- **No breaking changes**: one-shot `rdc sync <env>` unchanged. Watch is opt-in via `--watch`.
- **Lockfile format unchanged**.
- **New sibling lock file** (`.rdc/state/<env>.lock`) — auto-created on first use. Excluded from git via the existing `.rdc/` pattern in `.gitignore` (verify; add if missing).
- **No behavior change to existing one-shot flow** other than the advisory lock acquisition, which is invisible when nothing else is running.

## Open questions

- **Initial reconcile prompt behavior on first watch start.** A fresh `rdc sync --watch test` does the full one-shot reconcile first, including its plan-and-confirm prompt. Should that prompt be auto-confirmed when `--watch` is used, since the user is committing to long-running operation? Current spec preserves the prompt (matches one-shot). Could flip with a `--yes`-implied-by-`--watch` rule. Revisit during impl.
- **Poll-interval lower bound.** API rate limits are real (Rossum's documented limits are forgiving but not infinite). The spec accepts any `--poll-interval` value; should we floor at, say, 5s? Or warn below 10s? Out of scope for the first cut.
- **Windows behavior.** `notify` works on Windows; `fs4` works on Windows. Watch mode should function. But Windows file-event semantics differ (file moves, atomic renames). Verify in a follow-up; ship Linux/macOS first.
- **What "save" means for the file watcher.** Some editors write-then-rename (atomic save), others truncate-and-write. `notify` reports both as "modify." The 500ms debounce should handle both. Verify with vim, emacs, vscode during impl.
- **Daemon and `rdc upgrade` interaction.** If the daemon is running and the user runs `rdc upgrade`, the in-flight process keeps its old binary (Unix kernel keeps the file alive). New cycles from the running daemon use old logic. Acceptable for v1; document.

## Out of scope (deferred)

- **True backgrounded daemon (fork/detach).** Foreground only for this cut. A `rdc sync daemon` subcommand could land later if pull demand emerges; the watch mode here is the building block.
- **Sidecar resolve command.** Watch terminal IS the resolver UI.
- **OS-native notifications, system tray, sound.** Pure terminal output.
- **Multi-env watch in one process.** Open more terminal tabs.
- **Pause/resume signaling (SIGUSR1).** Use Ctrl-C and restart instead.
- **Persistent log file.** Output goes to the watch terminal; users redirect with shell tooling if they want a log.
- **Watching the lockfile or `.rdc/` internals.** Daemon-managed; users shouldn't touch them.
- **Detecting & reacting to git checkouts**. A `git checkout` to a branch with different snapshot bytes will fire a flurry of file events. Watch mode will treat them as ordinary edits and try to sync. This is the "right" behavior — the snapshot IS the source of truth — but for users who frequently switch branches, surprising. Mitigation deferred (could be a "git hook" install instruction in the README, or `--ignore-paths .git/HEAD-checkout-marker`).
