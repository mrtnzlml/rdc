# Watch-mode attention bell

**Date:** 2026-06-26
**Status:** Implemented + verified
**Area:** `rdc sync <env> --watch`

## Problem

`rdc sync <env> --watch` is a long-running, foreground, interactive loop
(`src/cli/sync/watch.rs`). When a cycle hits something that needs the user's
input — a merge conflict, a delete-vs-edit conflict, a remote-delete
reconciliation, a push-drift resolution, a destructive-delete drift, an MDH
prune, or an expired-token (401) re-auth — it blocks mid-cycle on a prompt and
waits.

If the user has stepped away, nothing pulls them back. The prompt sits silently
and the loop is stalled until they happen to look. A grep of `src/` confirms
there was no terminal bell, desktop notification, sound, or `osascript` /
`notify-rust` / `terminal-notifier` anywhere.

## Goal

Ring the **terminal bell** (BEL, `0x07`) the instant a watch cycle blocks on an
interactive prompt, so a stepped-away user is pulled back. The terminal turns
BEL into a sound and/or dock-bounce/notification — the "come back" signal we
want, with zero new dependencies and no per-platform code.

## Non-goals

- **No desktop-notification mechanism** (no `notify-rust`, no shelling out to
  `osascript`/`notify-send`/PowerShell). Keeps the self-contained,
  fat-LTO/stripped, rustls/glibc Homebrew binary posture intact. The bell is the
  whole feature.
- **No post-cycle / summary notification.** Conflicts are resolved *inline,
  mid-cycle*; we alert at the prompt, not after.
- **No coverage of non-interactive (`--yes` / non-TTY) deferrals.** Those defer
  without a blocking prompt; the bell is a TTY-only signal anyway.
- **No bell on non-blocking conditions** (transient errors, parse errors) — they
  are logged-and-skipped and do not wait for the user.

## Decisions

1. **Scenario:** interactive, stepped-away user.
2. **Mechanism:** terminal bell (BEL `0x07`) to stderr. Zero new dependencies.
3. **Scope:** *any* prompt that blocks a watch cycle for input.
4. **Enablement:** **on by default in interactive TTY watch**, with `--no-bell`
   to silence. Deliberate, accepted behavior change for interactive watch users;
   CI/non-TTY and all non-watch commands are unaffected.
5. **De-bounce:** **once per cycle**, on the first blocking prompt.

## Architecture — the `read_line_coordinated` chokepoint

> **Design history (honest):** the first implementation hooked the bell into
> `Log::with_prompt`. A pre-merge adversarial review proved that hook was
> *leaky*: not every blocking prompt is wrapped in `with_prompt` (the pull-driver
> conflict resolver and the delete-drift resolver bypassed it), and `with_prompt`
> *false-consumed* the armed flag when its closure didn't actually block (the
> `--allow-deletes` confirm). The design below replaced it with a single true
> chokepoint and was re-reviewed clean.

Every **coordinated** interactive prompt in a watch cycle funnels through one
function: `stdin_coord::read_line_coordinated()` (`src/cli/stdin_coord.rs`). In
watch mode the Enter-trigger reader is the sole stdin owner and hands prompt
input to waiting prompts through the coordinator; every resolver reads its line
via `read_line_coordinated` (directly or through the `CoordinatorStdin` BufRead
adapter). The Enter-trigger owner reads the real stdin directly and does **not**
call `read_line_coordinated`, so hooking it fires only for genuine prompts.

The bell is a process-global flag in `stdin_coord` (watch is a single foreground
process; the coordinator is already a process-global singleton):

```rust
static BELL_ARMED: AtomicBool = AtomicBool::new(false);

pub fn arm_bell() { BELL_ARMED.store(true, Ordering::Relaxed); }

// Split out for unit-testing the arm/debounce/re-arm/TTY-gate logic.
fn take_bell(is_tty: bool) -> bool {
    is_tty && BELL_ARMED.swap(false, Ordering::Relaxed)
}

pub fn maybe_ring_bell() {
    if take_bell(std::io::stderr().is_terminal()) {
        let mut err = std::io::stderr();
        let _ = err.write_all(b"\x07");
        let _ = err.flush();
    }
}
```

`read_line_coordinated()` calls `maybe_ring_bell()` at its top. Because this is
the actual moment a prompt blocks for input, it has **no false-consume**: a
non-prompting code path (e.g. `confirm_or_refuse` under `--allow-deletes`, which
returns before reading) never reaches it, so the armed flag survives to the
*next* real prompt.

### Every coordinated prompt reads through the coordinator

To get the bell *and* avoid the watch-deadlock that a raw `stdin().lock()` causes
(the Enter-trigger reader is the sole stdin owner), all interactive resolvers
read via `CoordinatorStdin` / `read_line_coordinated` — including two that
originally used a raw stdin lock and were switched as part of this work:

- **Push-drift** (`resolve::resolve_push_drift`).
- **Pull-driver combined-file conflict** (`resolve::resolve_combined_file`,
  reachable from the hook/rule/queue pull drivers — e.g. a mid-cycle drift during
  a portable-refs migration).

Both now ring via `read_line_coordinated` and no longer risk deadlocking watch.

### One non-coordinated prompt gets an explicit ring

The **401 token refresh** (`auth::refresh_token_for_401`) prompts via
`inquire::Password` (crossterm), which cannot use the coordinator, so it calls
`maybe_ring_bell()` directly before blocking. No-op outside watch (the global
flag is only armed by the watch loop) and off a TTY.

### Coverage map (verified)

| Blocking prompt | Reaches bell via |
|---|---|
| BothDiverged conflict (`execute::resolve_conflicts`) | `read_line_coordinated` (CoordinatorStdin) |
| Remote-delete / delete-vs-edit (`execute::resolve_remote_deletes`) | `read_line_coordinated` |
| Destructive-delete gate (`deletes::confirm_or_refuse`) | `read_line_coordinated` |
| **Destructive-delete drift** (`deletes::resolve_delete_drift`) | `read_line_coordinated` |
| MDH prune (`execute::prune_mdh_orphans`) | `read_line_coordinated` |
| MDH push (`push::mdh`) | `read_line_coordinated` |
| **Pull-driver flat conflict** (`pull::common::resolve_conflict_interactive`) | `read_line_coordinated` (CoordinatorStdin) |
| **Pull-driver combined-file conflict** (`resolve::resolve_combined_file`) | `read_line_coordinated` (CoordinatorStdin) |
| Push-drift (`resolve::resolve_push_drift`) | `read_line_coordinated` (CoordinatorStdin) |
| 401 token (`auth::refresh_token_for_401`) | explicit `maybe_ring_bell()` |

The bold rows are the prompts the reviews found bypassing the old `with_prompt`
hook (`resolve_conflict_interactive` and `resolve_delete_drift` in the first
review; `resolve_combined_file` in the re-review). Under the chokepoint design
every coordinated prompt rings by construction — no per-site wrapping — and any
future one does too. Routing `resolve_combined_file` and `resolve_push_drift`
through the coordinator also fixed a latent watch deadlock (a raw `stdin().lock()`
competing with the Enter-trigger reader). Only the 401 inquire prompt, which
cannot use the coordinator, rings explicitly.

### Arming

The watch loop arms the global flag once per cycle, gated on `!no_bell`:

- `run_watch`, before the initial reconcile `run_cycle` (`watch.rs`).
- `event_loop`, before each steady-state `run_cycle` (`watch.rs`).

`maybe_ring_bell` consumes the flag on the first blocking prompt (via `swap`), so
the bell rings once per cycle; re-arming every cycle is required (arming once
would ring only the first prompt ever).

## Backward compatibility

- **CI / non-TTY:** unchanged. The emit is gated on `stderr().is_terminal()`,
  and `&&` short-circuit means a non-TTY never even consumes the armed flag.
- **All non-watch commands:** unchanged. Only the watch loop calls `arm_bell()`,
  so `maybe_ring_bell()` is a no-op everywhere else (one-shot sync, deploy,
  doctor, the generic `with_401_retry`, etc.).
- **`--yes` / non-interactive watch:** unchanged. No blocking prompt fires.
- **No new dependencies, no per-platform code, no binary-size change.**
  `dead_code = "deny"` satisfied — flag, `arm_bell`, `take_bell`,
  `maybe_ring_bell`, and the `--no-bell` flag are all used.
- Internal signature change only: `run_watch` / `event_loop` gain `no_bell: bool`
  (binary-crate-internal). No public API change; `Log` is untouched.

## CLI surface

`--no-bell` on the `Sync` subcommand (`requires = "watch"`), threaded through
`run_watch` → `event_loop`:

```rust
/// Silence the terminal bell that watch mode rings when a cycle blocks for
/// input (conflict / delete / drift / token prompt). On by default on a TTY.
#[arg(long = "no-bell", requires = "watch")]
no_bell: bool,
```

## Testing

### Unit
- `stdin_coord::tests::bell_arm_take_debounce_rearm_and_tty_gate` — exercises
  `take_bell` for: off-TTY never consumes, armed+TTY rings once, debounce,
  re-arm rings again, unarmed silent. (The byte write itself is PTY-verified
  below, since a unit test's stderr is not a TTY.)

### Real-PTY (manual, repeatable)
A throwaway example driving `arm_bell` + `maybe_ring_bell` through the compiled
binary, run under `script -q`, emitted **exactly 2 BEL (0x07)** for
arm→ring→debounce→re-arm→ring, and **0** when piped (stderr → file). This proves
the production write + `is_terminal` gate on a real terminal (the `tick_status`
non-TTY caveat applies — a piped run correctly shows nothing). Verify against the
locally built binary, not the Homebrew one.

### Suite
Full `cargo test` green (769 lib + integration, 0 failed). `--no-bell` appears in
`sync --help` and clap rejects it without `--watch` (`requires = "watch"`).

## Files touched

- `src/cli/stdin_coord.rs` — `BELL_ARMED`, `arm_bell`, `take_bell`,
  `maybe_ring_bell`; `maybe_ring_bell()` call at the top of
  `read_line_coordinated`; unit test.
- `src/cli/sync/watch.rs` — `no_bell` param on `run_watch` + `event_loop`;
  `stdin_coord::arm_bell()` before the initial and steady-state cycles, gated on
  `!no_bell`.
- `src/cli/mod.rs` — `--no-bell` flag on `Sync`; destructure; pass to
  `run_watch`.
- `src/cli/resolve.rs` — route `resolve_push_drift` and `resolve_combined_file`
  through `CoordinatorStdin` (they ring via `read_line_coordinated` and no longer
  deadlock under watch).
- `src/cli/auth.rs` — `maybe_ring_bell()` before the 401 token prompt.
