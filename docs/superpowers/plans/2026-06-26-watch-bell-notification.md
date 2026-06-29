# Watch-mode Attention Bell Implementation Plan

> **SUPERSEDED hook mechanism (kept for history):** this plan implemented the
> bell via `Log::with_prompt`. A pre-merge adversarial review found that hook was
> leaky (two blocking prompts bypassed it; it false-consumed under
> `--allow-deletes`). The shipped design moved the bell to the
> `stdin_coord::read_line_coordinated` chokepoint plus explicit rings for the two
> non-coordinated prompts. See the design doc
> `docs/superpowers/specs/2026-06-26-watch-bell-notification-design.md` for the
> final architecture. The Task structure / TDD cadence / verification approach
> below still describe the process used.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ring the terminal bell (BEL `0x07`) when `rdc sync <env> --watch` blocks on an interactive prompt, so a stepped-away user is pulled back.

**Architecture:** A single armed-flag (`bell_armed: AtomicBool`) on the shared `Log`. The watch loop arms it once per cycle; `Log::with_prompt` (the chokepoint every interactive prompt passes through) emits one BEL on the first prompt while armed, gated on `is_tty`. Two prompts that currently bypass `with_prompt` — push-drift (`resolve_push_drift`) and the per-cycle 401 token prompt (`refresh_token_for_401`) — are routed through it so coverage is complete. On by default on a TTY; silenced with `--no-bell`.

**Tech Stack:** Rust (edition 2024), tokio async, clap, inquire. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-06-26-watch-bell-notification-design.md`

## Global Constraints

- **No new dependencies.** BEL is a single `0x07` byte; nothing else is needed.
- **`[workspace.lints.rust] dead_code = "deny"`** — every added field/method/param/flag must be used, or the build fails.
- **No repo-wide `cargo fmt`.** The repo is not rustfmt-clean; a repo-wide fmt is a known pre-existing skew. Format only the lines you touch, by hand if needed.
- **No customer names / customer data** anywhere (code, tests, commit messages). Use neutral placeholders.
- **Backward compatibility:** non-TTY/CI and all non-watch commands must be unaffected. BEL only on `is_tty`; arming only from the watch path + watch-cycle 401 handler.
- **Edition 2024**, Rust; async runtime is tokio.

---

### Task 1: Core bell mechanism in `Log`

**Files:**
- Modify: `src/log.rs` — struct field (`Log`, ~line 271), 4 constructors (`new` ~291, `for_test` ~307, `for_test_with_time` ~324, `for_test_tty_with_time` ~341), `with_prompt` (~444), top-of-file imports.
- Test: `src/log.rs` `#[cfg(test)] mod tests` (~line 505+).

**Interfaces:**
- Produces: `Log::arm_bell(&self)` — sets the armed flag. `with_prompt` emits one `0x07` to the sink on the first call after arming, only when `is_tty`, then disarms.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/log.rs` (the `Buf` sink and `for_test*` helpers already exist there):

```rust
    #[test]
    fn bell_rings_once_when_armed_on_tty() {
        let buf = Buf::default();
        let log = Log::for_test_tty_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600),
        );
        log.arm_bell();
        log.with_prompt(|| ());
        assert_eq!(buf.text().matches('\u{7}').count(), 1, "expected exactly one BEL");
    }

    #[test]
    fn bell_debounces_within_a_single_arm() {
        let buf = Buf::default();
        let log = Log::for_test_tty_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600),
        );
        log.arm_bell();
        log.with_prompt(|| ());
        log.with_prompt(|| ());
        assert_eq!(buf.text().matches('\u{7}').count(), 1, "second prompt must not re-ring");
    }

    #[test]
    fn bell_rings_again_after_rearm() {
        let buf = Buf::default();
        let log = Log::for_test_tty_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600),
        );
        log.arm_bell();
        log.with_prompt(|| ());
        log.arm_bell();
        log.with_prompt(|| ());
        assert_eq!(buf.text().matches('\u{7}').count(), 2, "re-arm must ring again");
    }

    #[test]
    fn bell_silent_without_arming() {
        let buf = Buf::default();
        let log = Log::for_test_tty_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600),
        );
        log.with_prompt(|| ());
        assert_eq!(buf.text().matches('\u{7}').count(), 0, "unarmed prompt must be silent");
    }

    #[test]
    fn bell_silent_off_tty_even_when_armed() {
        let buf = Buf::default();
        let log = Log::for_test(ColorMode::Plain, Box::new(buf.clone()));
        log.arm_bell();
        log.with_prompt(|| ());
        assert_eq!(buf.text().matches('\u{7}').count(), 0, "non-TTY must never ring");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib log::tests::bell_ 2>&1 | tail -20`
Expected: FAIL — compile error `no method named 'arm_bell' found for ... Log` (the method does not exist yet).

- [ ] **Step 3: Implement the mechanism**

3a. Add the atomic import near the top of `src/log.rs` (with the other `use` lines). If `std::sync::atomic` is not already imported, add:

```rust
use std::sync::atomic::{AtomicBool, Ordering};
```

3b. Add the field to the `Log` struct (after the `phase` field, before the `#[cfg(test)] fixed_time` field):

```rust
    /// Watch-mode attention bell. When `true`, the next `with_prompt` on a
    /// TTY emits a BEL (0x07) and resets this to `false`. Armed once per
    /// watch cycle by `arm_bell`; never set outside the watch path.
    bell_armed: AtomicBool,
```

3c. Initialize it in **all four** constructors. In each `Self { ... }` literal, add the line `bell_armed: AtomicBool::new(false),`:
- in `new` (the `Arc::new(Self { ... })` at ~line 291),
- in `for_test` (~307),
- in `for_test_with_time` (~324),
- in `for_test_tty_with_time` (~341).

3d. Add the `arm_bell` method inside `impl Log` (place it right after `with_prompt`):

```rust
    /// Arm the watch attention bell: the next `with_prompt` on a TTY emits a
    /// BEL (0x07), then disarms. The watch loop re-arms once per cycle.
    pub fn arm_bell(&self) {
        self.bell_armed.store(true, Ordering::Relaxed);
    }
```

3e. Emit the BEL in `with_prompt`. Change the body from:

```rust
        let mut state = self.state.lock().unwrap();
        if state.status_active {
            let _ = state.out.write_all(b"\r\x1b[K");
            state.status_active = false;
        }
        let _ = state.out.flush();
        drop(state);
        f()
```

to:

```rust
        let mut state = self.state.lock().unwrap();
        if state.status_active {
            let _ = state.out.write_all(b"\r\x1b[K");
            state.status_active = false;
        }
        // Watch-mode attention bell: ring once per armed cycle so an
        // away-from-keyboard user is pulled back. TTY-gated like tick_status;
        // `&&` short-circuit means a non-TTY never consumes the armed flag.
        if self.is_tty && self.bell_armed.swap(false, Ordering::Relaxed) {
            let _ = state.out.write_all(b"\x07");
        }
        let _ = state.out.flush();
        drop(state);
        f()
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib log::tests::bell_ 2>&1 | tail -20`
Expected: PASS — all 5 `bell_*` tests green.

- [ ] **Step 5: Commit**

```bash
git add src/log.rs
git commit -m "feat(log): add watch-mode attention bell to with_prompt

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `--no-bell` flag and per-cycle arming

**Files:**
- Modify: `src/cli/mod.rs` — `Sync` subcommand struct (~line 144, after `verbose`), the `Command::Sync` destructure (~line 286–296), the `run_watch` call (~306–314).
- Modify: `src/cli/sync/watch.rs` — `run_watch` signature (~line 60–68), initial-cycle arming (~line 78), the `event_loop` call (~210–224), `event_loop` signature (~256–270), steady-state arming (~before line 323).

**Interfaces:**
- Consumes: `Log::arm_bell` (Task 1).
- Produces: `run_watch(env, interactive, allow_deletes, no_push, no_pull, poll_interval, verbose, no_bell)` and `event_loop(..., verbose, no_bell, events, shutdown, watcher, env_root, renderer, sync_running, timer_reset)` — both gain a trailing/`verbose`-adjacent `no_bell: bool`. Task 4 reads `no_bell` inside `event_loop`.

- [ ] **Step 1: Add the `--no-bell` flag to the `Sync` subcommand**

In `src/cli/mod.rs`, after the `verbose` field (~line 144) inside `Sync { ... }`:

```rust
        /// Silence the terminal bell that watch mode rings when a cycle blocks
        /// for input (conflict / delete / drift / token prompt). On by default
        /// on a TTY.
        #[arg(long = "no-bell", requires = "watch")]
        no_bell: bool,
```

- [ ] **Step 2: Thread `no_bell` through the `Sync` handler**

In `src/cli/mod.rs`, add `no_bell,` to the `Some(Command::Sync { ... })` destructure (~line 286–296), and pass it as the last argument of the `run_watch` call (~306–314):

```rust
                with_401_retry(&env, || {
                    crate::cli::sync::watch::run_watch(
                        &env,
                        interactive,
                        allow_deletes,
                        no_push,
                        no_pull,
                        poll,
                        verbose,
                        no_bell,
                    )
                })
                .await
```

- [ ] **Step 3: Add `no_bell` to `run_watch` and arm the initial cycle**

In `src/cli/sync/watch.rs`, add `no_bell: bool,` to the `run_watch` signature (after `verbose: bool,`, ~line 67). Then in the initial-reconcile block (~line 76–91), arm before `run_cycle`:

```rust
    {
        let _lock =
            crate::cli::sync::lock::EnvLock::acquire(&paths.env_lock(), Duration::from_secs(30))?;
        if !no_bell {
            renderer.arm_bell();
        }
        crate::cli::sync::run_cycle(
            env,
            interactive,
            false,
            allow_deletes,
            no_push,
            no_pull,
            Some(renderer.clone()),
            None,
            None,
        )
        .await?;
    }
```

- [ ] **Step 4: Pass `no_bell` to `event_loop` and add it to the signature**

In `src/cli/sync/watch.rs`, add `no_bell,` to the `event_loop(...)` call (~after `verbose,` at line 216), and add `no_bell: bool,` to the `event_loop` signature (after `verbose: bool,`, ~line 262).

- [ ] **Step 5: Arm the steady-state cycle**

In `event_loop`, immediately before the `let _outcome = match crate::cli::sync::run_cycle(` line (~323), insert:

```rust
                if !no_bell {
                    if let Some(r) = renderer.as_ref() {
                        r.arm_bell();
                    }
                }
```

- [ ] **Step 6: Fix the existing `event_loop` test call sites**

The build will fail with "this function takes N arguments" at each test that calls `event_loop(...)`. Find them:

Run: `grep -rn "event_loop(" src/cli/sync/watch.rs`

For each test call site, add `false,` in the `no_bell` position (immediately after the `verbose` argument). `false` = bell enabled (default); the tests don't assert on the bell, so the value is immaterial, but matching the production default keeps them representative.

- [ ] **Step 7: Build and run the watch tests**

Run: `cargo build 2>&1 | tail -20`
Expected: clean build (no `dead_code` error — `no_bell` is used by the arming guards).

Run: `cargo test --lib sync::watch 2>&1 | tail -20`
Expected: PASS — existing `event_loop` tests still green.

- [ ] **Step 8: Commit**

```bash
git add src/cli/mod.rs src/cli/sync/watch.rs
git commit -m "feat(watch): --no-bell flag and per-cycle bell arming

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Route push-drift through `with_prompt`

**Files:**
- Modify: `src/cli/resolve.rs` — `resolve_push_drift` signature + body (~line 1292), its unit test (~line 2336).
- Modify: `src/cli/push/{hooks,rules,schemas,queues,inboxes,workspaces,engines,engine_fields,labels,email_templates}.rs` — the single `resolve_push_drift(...)` call in each (10 sites).

**Interfaces:**
- Consumes: `Log::with_prompt` (already exists; bell emit from Task 1).
- Produces: `resolve_push_drift(log: &Log, interactive: bool, local_path: &Path, remote_bytes: &[u8], env: &str) -> Result<PushDriftOutcome>` — gains a leading `&Log`.

- [ ] **Step 1: Update the existing test for the new signature (failing first)**

In `src/cli/resolve.rs`, change `resolve_push_drift_non_interactive_returns_skip` (~line 2336) to pass a `Log`:

```rust
    #[test]
    fn resolve_push_drift_non_interactive_returns_skip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obj.json");
        std::fs::write(&path, b"local\n").unwrap();
        let log = crate::log::Log::new(ColorMode::Plain);
        let r = resolve_push_drift(&log, false, &path, b"remote\n", "test").unwrap();
        assert!(matches!(r, PushDriftOutcome::Skip));
    }
```

(Keep whatever setup the existing test uses; the only required change is adding `let log = ...;` and the `&log,` first argument. `ColorMode` is defined in this file. `Log::new` returns `Arc<Log>`; `&log` deref-coerces to `&Log`.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib resolve_push_drift_non_interactive 2>&1 | tail -20`
Expected: FAIL — compile error `this function takes 4 arguments but 5 ... were supplied` (signature not updated yet).

- [ ] **Step 3: Update `resolve_push_drift` to take `&Log` and wrap its prompt**

In `src/cli/resolve.rs`, change the signature (~line 1292) to add `log: &crate::log::Log,` as the **first** parameter, and wrap the `prompt_resolve` call in `log.with_prompt`:

```rust
pub fn resolve_push_drift(
    log: &crate::log::Log,
    interactive: bool,
    local_path: &Path,
    remote_bytes: &[u8],
    env: &str,
) -> Result<PushDriftOutcome> {
    if !interactive {
        return Ok(PushDriftOutcome::Skip);
    }

    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    let resolution = log.with_prompt(|| {
        prompt_resolve(
            stdin.lock(),
            stderr.lock(),
            1,
            1,
            local_path,
            remote_bytes,
            env,
        )
    })?;
    match resolution {
        // ... unchanged arms ...
    }
}
```

(Only the signature's new first param and the `log.with_prompt(|| { ... })` wrapper around the existing `prompt_resolve(...)` call change. The `match resolution { ... }` block is untouched.)

- [ ] **Step 4: Update the 10 push-driver call sites**

Each driver calls `resolve_push_drift(interactive, <path>, <bytes>, env)` and has a `progress: &Arc<Log>` parameter in scope. Add `progress,` as the first argument. Locations (verify line with grep — they shift as you edit):

Run: `grep -rn "resolve_push_drift(" src/cli/push/`

For each of the 10 (`hooks.rs:298`, `rules.rs:114`, `schemas.rs:110`, `queues.rs:136`, `inboxes.rs:107`, `workspaces.rs:119`, `engines.rs:142`, `engine_fields.rs:127`, `labels.rs:124`, `email_templates.rs:232`), change e.g.:

```rust
            match resolve_push_drift(interactive, local_json_path, &remote_json, env)? {
```

to:

```rust
            match resolve_push_drift(progress, interactive, local_json_path, &remote_json, env)? {
```

(`progress` is `&Arc<Log>`, which deref-coerces to the `&Log` parameter. Keep each site's existing path/bytes argument names — they differ per driver, e.g. `path`/`&remote_bytes`, `schema_path`/`&remote_json`.)

- [ ] **Step 5: Build and run the test**

Run: `cargo build 2>&1 | tail -20`
Expected: clean build.

Run: `cargo test --lib resolve_push_drift_non_interactive 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cli/resolve.rs src/cli/push/
git commit -m "fix(sync): route push-drift prompt through Log::with_prompt

Centralizes the prompt in the shared resolver so the watch bell rings on
push-drift and the renderer suspends consistently with conflict prompts.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Ring on the per-cycle 401 token prompt

**Files:**
- Modify: `src/cli/auth.rs` — `refresh_token_for_401` signature (~line 197) + its `inquire::Password` prompt loop (~line 250–264).
- Modify: `src/cli/sync/watch.rs` — the inner 401 handler call (~line 336).
- Modify: `src/cli/mod.rs` — the generic `with_401_retry` call (~line 378).

**Interfaces:**
- Consumes: `Log::arm_bell`, `Log::with_prompt` (Task 1); `no_bell` in `event_loop` scope (Task 2).
- Produces: `refresh_token_for_401(env: &str, ring_bell: bool) -> Result<()>`.

- [ ] **Step 1: Add `ring_bell` to `refresh_token_for_401` and wrap its prompt**

In `src/cli/auth.rs`, change the signature (~line 197) to:

```rust
pub async fn refresh_token_for_401(env: &str, ring_bell: bool) -> Result<()> {
```

The function already builds `let log = crate::log::Log::new(...)`. After that `log` is created (and after its existing `log.event(Action::Auth, ...)` line), and immediately before the `loop {`, add:

```rust
    if ring_bell {
        log.arm_bell();
    }
```

Then wrap the `Password` prompt in `log.with_prompt`. Change:

```rust
        let new_token = match Password::new("New API token")
            .with_display_mode(PasswordDisplayMode::Masked)
            .without_confirmation()
            .with_help_message("Ctrl+C to cancel")
            .prompt()
        {
```

to:

```rust
        let new_token = match log.with_prompt(|| {
            Password::new("New API token")
                .with_display_mode(PasswordDisplayMode::Masked)
                .without_confirmation()
                .with_help_message("Ctrl+C to cancel")
                .prompt()
        }) {
```

(The `Ok(s) => s,` / `Err(...) => ...` arms are unchanged. `with_prompt` returns the closure's `Result<String, InquireError>`.)

- [ ] **Step 2: Update the inner watch 401 handler to ring**

In `src/cli/sync/watch.rs` (~line 336), change:

```rust
                        crate::cli::auth::refresh_token_for_401(env).await?;
```

to:

```rust
                        crate::cli::auth::refresh_token_for_401(env, !no_bell).await?;
```

(`no_bell` is in scope from Task 2. `!no_bell` = bell enabled.)

- [ ] **Step 3: Update the generic `with_401_retry` to not ring**

In `src/cli/mod.rs` (~line 378), change:

```rust
            crate::cli::auth::refresh_token_for_401(env).await?;
```

to:

```rust
            crate::cli::auth::refresh_token_for_401(env, false).await?;
```

(Generic retry path used by non-watch commands; never rings.)

- [ ] **Step 4: Build and run the auth tests**

Run: `cargo build 2>&1 | tail -20`
Expected: clean build (both `refresh_token_for_401` call sites updated; `ring_bell` used).

Run: `cargo test --lib auth 2>&1 | tail -20`
Expected: PASS (or "0 tests" if `auth` has none — no failures).

- [ ] **Step 5: Commit**

```bash
git add src/cli/auth.rs src/cli/sync/watch.rs src/cli/mod.rs
git commit -m "feat(watch): ring attention bell on per-cycle 401 token prompt

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Full build, test, and manual PTY verification

**Files:** none (verification only).

- [ ] **Step 1: Full workspace build**

Run: `cargo build 2>&1 | tail -20`
Expected: clean build, no warnings (`dead_code = "deny"` would fail on any unused addition).

- [ ] **Step 2: Full test suite**

Run: `cargo test 2>&1 | tail -30`
Expected: all tests pass, including the 5 new `bell_*` tests and the updated `resolve_push_drift` test. (Do **not** run `cargo fmt`.)

- [ ] **Step 3: Confirm `--no-bell` is wired and watch-only**

Run: `cargo run -- sync --help 2>&1 | grep -A1 "no-bell"`
Expected: the `--no-bell` flag appears with its help text.

Run: `cargo run -- sync --no-bell some-env 2>&1 | tail -5`
Expected: clap rejects it with an error noting `--no-bell` requires `--watch` (proves the `requires = "watch"` gate). (`some-env` need not exist; the clap validation fires before env resolution.)

- [ ] **Step 4: Manual PTY verification (the real end-to-end check)**

The bell is TTY-gated, so a plain piped run shows nothing — verify under a PTY, against the locally built binary (Homebrew `rdc` shadows local builds on PATH; invoke `./target/debug/rdc` or `cargo run --` explicitly).

In a real project dir with a watchable env, create a divergence that forces a conflict prompt (edit a local object so both local and remote differ since the lockfile base), then:

```bash
script -q /dev/null ./target/debug/rdc sync <env> --watch
```

Expected: when the conflict prompt appears, the terminal emits an audible bell / dock-bounce (terminal-config dependent). Then repeat with `--no-bell`:

```bash
script -q /dev/null ./target/debug/rdc sync <env> --watch --no-bell
```

Expected: the same prompt appears with **no** bell. Finally, a piped (non-TTY) run never rings:

```bash
./target/debug/rdc sync <env> --watch 2>&1 | cat
```

Expected: no `0x07` in the stream (cycle runs, deferrals go to shadow files, no bell). Document the observed results.

- [ ] **Step 5: Final summary**

No commit needed (verification only). Report: build status, test count, and the three manual observations (bell on conflict, silent with `--no-bell`, silent when piped).

---

## Self-Review

**Spec coverage:**
- Mechanism (BEL in `with_prompt`, `bell_armed`, `arm_bell`) → Task 1. ✓
- On-by-default + `--no-bell` + per-cycle arming → Task 2. ✓
- Fix 1 push-drift routing → Task 3. ✓
- Fix 2 401 prompt → Task 4. ✓
- Three gates (watch-only / opt-out / TTY) → enforced across Tasks 1–2 (arm only in watch; `!no_bell` guard; `is_tty` in `with_prompt`); tested in Task 1 (off-TTY silent) and Task 5 (`--no-bell` silent, piped silent). ✓
- Once-per-cycle de-bounce → Task 1 tests (debounce + re-arm). ✓
- Backward-compat (non-TTY/CI, non-watch) → Task 1 off-TTY test + Task 5 piped run + the watch-only arming. ✓
- Testing (5 unit tests, updated push-drift test, PTY manual) → Tasks 1, 3, 5. ✓

**Placeholder scan:** No TBD/TODO; every code step shows the exact change. Per-driver argument names are noted as varying, with grep to locate them — not a placeholder, an instruction to match existing local names. ✓

**Type consistency:** `arm_bell(&self)`, `bell_armed: AtomicBool`, `Ordering::Relaxed`, `with_prompt` BEL emit, `resolve_push_drift(log: &Log, ...)`, `refresh_token_for_401(env, ring_bell: bool)`, `run_watch(..., no_bell: bool)`, `event_loop(..., no_bell: bool)` — names match across all tasks. ✓
