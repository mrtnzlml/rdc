# Sync Watch Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `rdc sync --watch <env>` — a foreground watch mode that keeps an env continuously reconciled via file-system events + periodic remote poll, prompting inline for conflicts and destructive deletes.

**Architecture:** A `cli::sync::watch::run_watch` entry point spawns three event sources (file watcher via `notify`, poll timer via `tokio::time::interval`, ctrl-c handler) feeding a single `mpsc::channel<CycleTrigger>`. A worker task drains the channel; on each event it acquires an advisory lock on `.rdc/state/<env>.lock` (via `fs4`), runs the existing `sync::run_cycle` (extracted from `sync::run`), releases the lock, and prints a one-line outcome. Plan-and-confirm is suppressed for non-destructive cycles; conflicts and destructive deletes still use the existing inline resolver prompts.

**Tech Stack:** Rust (existing rdc codebase), `tokio` (existing — add `signal` feature), `notify` (new), `fs4` (new), `serde_json`, `reqwest`, `wiremock` for tests. No daemon, fork, or IPC machinery.

**Spec:** `docs/superpowers/specs/2026-05-14-watch-mode-design.md` — read it before starting.

---

## File Structure

**Create:**
- `src/cli/sync/lock.rs` — `EnvLock` RAII guard wrapping `fs4` advisory locks. Acquire with timeout; release on drop. Lock file lives at `Paths::env_lock()` (new helper).
- `src/cli/sync/watch.rs` — public `run_watch` plus a testable `event_loop`. Includes `CycleTrigger` enum, file-watcher setup, poll-timer task, ctrl-c handler, debouncing, watcher pause/resume.

**Modify:**
- `Cargo.toml` — add `notify = "6"` and `fs4 = "0.9"` (or current stable); add `signal` to tokio features.
- `src/paths.rs` — add `Paths::env_lock()` returning `.rdc/state/<env>.lock`.
- `src/cli/sync/mod.rs` — split current `pub async fn run` into two pieces: `pub async fn run` (one-shot, acquires lock, calls `run_cycle`) and `pub(crate) async fn run_cycle` (the pipeline body, no lock acquisition). Add `auto_confirm_non_destructive: bool` parameter to `run_cycle`.
- `src/cli/mod.rs` — extend the `Sync` variant with `--watch`, `--poll-interval`, `--no-poll`, `-v` flags; reject `--watch --dry-run` and `--watch --diff` at parse time; route to `cli::sync::watch::run_watch` when `--watch` is present.
- `src/cli/deploy/run.rs` — acquire the target env's lock for its write phase (apply + bootstrap).
- `tests/cli_sync.rs` — add new integration tests for the watch flow (use a synthetic shutdown channel to terminate the loop deterministically).
- `README.md` — add `rdc sync --watch` to the Commands table and a short "Watch mode" subsection under "Promote test → prod" or similar.

**No deletes.**

---

## Task 1: `EnvLock` RAII guard

**Goal:** A blocking-with-timeout advisory lock acquired via `fs4`, released on drop. Used by sync (one-shot and watch) and deploy to serialize writers per env.

**Files:**
- Create: `src/cli/sync/lock.rs`.
- Modify: `Cargo.toml` (add `fs4`).
- Modify: `src/cli/sync/mod.rs` (re-export `lock` module).
- Modify: `src/paths.rs` (add `env_lock` helper).

- [ ] **Step 1: Add `fs4` dependency**

In `Cargo.toml`'s `[dependencies]` block, add (matching the alphabetical convention of nearby entries):

```toml
fs4 = "0.9"
```

Run `cargo build` to confirm it resolves.

- [ ] **Step 2: Add `Paths::env_lock`**

In `src/paths.rs`, after the existing `lockfile()` method (around line 41):

```rust
/// `<root>/.rdc/state/<env>.lock` — advisory lock file (sibling of the
/// JSON lockfile content). Empty file; existence is incidental. Used by
/// `EnvLock` for cross-process write serialization.
pub fn env_lock(&self) -> PathBuf {
    self.root
        .join(".rdc")
        .join("state")
        .join(format!("{}.lock", self.env))
}
```

Then in the existing `tests` module at the bottom of `src/paths.rs`, add a test next to `lockfile_path`:

```rust
#[test]
fn env_lock_path() {
    assert_eq!(p().env_lock(), Path::new("/proj/.rdc/state/dev.lock"));
}
```

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p rdc --lib paths::tests::env_lock_path -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Write failing tests for `EnvLock`**

Create `src/cli/sync/lock.rs`:

```rust
//! Advisory cross-process lock for serializing writers on one env.
//!
//! Sync (one-shot and watch) and deploy (writes to target env) acquire
//! this lock for the duration of their execute phase. The lock file
//! lives at `Paths::env_lock()` — sibling of the JSON lockfile.
//!
//! Crash safety: the OS releases the lock on process exit. The empty
//! lock file is left behind; subsequent runs reuse it.

use anyhow::{Context, Result};
use fs4::FileExt;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::Duration;

pub struct EnvLock {
    file: File,
}

impl EnvLock {
    /// Acquire an exclusive lock, waiting up to `timeout`. Polls every
    /// 200 ms while blocked. Creates the lock file (and the parent
    /// directory) if needed.
    pub fn acquire(lock_path: &Path, timeout: Duration) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating lock parent dir {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .with_context(|| format!("opening lock file {}", lock_path.display()))?;

        let deadline = std::time::Instant::now() + timeout;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(EnvLock { file }),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "timed out after {:?} waiting for env lock at {}",
                            timeout,
                            lock_path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("acquiring exclusive lock on {}", lock_path.display())
                    });
                }
            }
        }
    }
}

impl Drop for EnvLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn acquire_succeeds_on_unheld_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        let _lock = EnvLock::acquire(&path, Duration::from_secs(1)).unwrap();
        assert!(path.exists(), "lock file should be created");
    }

    #[test]
    fn second_acquire_times_out_when_first_held() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        let _first = EnvLock::acquire(&path, Duration::from_secs(1)).unwrap();
        let err = EnvLock::acquire(&path, Duration::from_millis(300)).unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
    }

    #[test]
    fn second_acquire_succeeds_after_first_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        let barrier = Arc::new(Barrier::new(2));
        let path2 = path.clone();
        let b2 = barrier.clone();
        let handle = thread::spawn(move || {
            let lock = EnvLock::acquire(&path2, Duration::from_secs(1)).unwrap();
            b2.wait(); // signal that we have the lock
            thread::sleep(Duration::from_millis(200));
            drop(lock);
        });
        barrier.wait();
        let _second = EnvLock::acquire(&path, Duration::from_secs(2))
            .expect("should acquire after first thread drops");
        handle.join().unwrap();
    }
}
```

- [ ] **Step 5: Wire the module**

In `src/cli/sync/mod.rs`, add near the other `pub mod` lines:

```rust
pub mod lock;
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p rdc --lib cli::sync::lock::tests -- --nocapture`
Expected: 3 passes.

Also run the full suite: `cargo test -p rdc`. Expected: all pass (no regressions).

- [ ] **Step 7: Stage**

```bash
git add Cargo.toml Cargo.lock src/paths.rs src/cli/sync/mod.rs src/cli/sync/lock.rs
```

Do not commit; the controller commits on your behalf.

---

## Task 2: Extract `sync::run_cycle` from `sync::run`

**Goal:** Move the body of `sync::run` (lockfile load, list_remote, scan, classify, plan, execute, save) into a `run_cycle` helper. `run` becomes a thin wrapper: acquire `EnvLock`, call `run_cycle`, release. No behavior change.

**Files:**
- Modify: `src/cli/sync/mod.rs`.

- [ ] **Step 1: Read the current `sync::run`**

Open `src/cli/sync/mod.rs`. Identify the body of `pub async fn run` — from project config load through `_index.md` generation. This becomes `run_cycle`'s body.

- [ ] **Step 2: Define `CycleOutcome` and extract `run_cycle`**

Replace the existing `pub async fn run` with:

```rust
/// Aggregate counts from one sync cycle, used for the watch-loop summary line.
/// For one-shot sync, callers usually don't read this — they look at the
/// printed summary instead.
#[derive(Debug, Default)]
pub struct CycleOutcome {
    pub items_pushed: usize,
    pub items_pulled: usize,
    pub conflicts: usize,
    pub remote_deletes_resolved: usize,
}

/// One reconciliation pass: list remote, scan local, classify, prompt-if-needed,
/// execute, save. Caller is responsible for holding the env lock for the
/// duration of this call.
///
/// `auto_confirm_non_destructive`: when true (watch mode), the plan-and-confirm
/// prompt is suppressed for cycles with zero conflicts and zero destructive
/// items. Conflicts and destructive deletes still prompt.
pub(crate) async fn run_cycle(
    env: &str,
    interactive: bool,
    dry_run: bool,
    diff: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    auto_confirm_non_destructive: bool,
) -> Result<CycleOutcome> {
    // -- everything that was previously inside `pub async fn run`, except
    //    the lock acquisition (now done by callers) --

    // ... existing body from `let cwd = ...` through `crate::cli::index::generate(...)?` ...

    // At the end, return CycleOutcome (default for now; real counters
    // populated in Task 16 if we wire them through. For this task,
    // returning Default::default() is acceptable.)
    Ok(CycleOutcome::default())
}

pub async fn run(
    env: &str,
    interactive: bool,
    dry_run: bool,
    diff: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
) -> Result<()> {
    // One-shot wrapper. Watch mode goes through `cli::sync::watch::run_watch`.
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = crate::paths::Paths::for_env(&cwd, env);
    let _lock = crate::cli::sync::lock::EnvLock::acquire(
        &paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;
    run_cycle(env, interactive, dry_run, diff, allow_deletes, no_push, no_pull, false).await?;
    Ok(())
}
```

The plan-and-confirm block inside `run_cycle` must consult `auto_confirm_non_destructive`. Find the existing confirmation logic (`if interactive && !classified.is_empty() && !confirm("Proceed?")?`); wrap it:

```rust
let has_conflicts_or_destructive = classified.iter().any(|c| {
    matches!(
        c.class,
        crate::cli::sync::classify::SyncClass::BothDiverged
            | crate::cli::sync::classify::SyncClass::LocalEditRemoteDelete
            | crate::cli::sync::classify::SyncClass::LocalDeleteRemoteEdit
            | crate::cli::sync::classify::SyncClass::RemoteDelete
            | crate::cli::sync::classify::SyncClass::LocalDelete
    )
});
let should_confirm = interactive
    && !classified.is_empty()
    && !(auto_confirm_non_destructive && !has_conflicts_or_destructive);
if should_confirm && !confirm("Proceed?")? {
    eprintln!("sync aborted by user.");
    return Ok(CycleOutcome::default());
}
```

- [ ] **Step 3: Run the suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS — all 482 tests still green. Pure refactor.

- [ ] **Step 4: Stage**

```bash
git add src/cli/sync/mod.rs
```

---

## Task 3: Deploy acquires the target env's lock

**Goal:** Prevent deploy and watch from racing on the same target env.

**Files:**
- Modify: `src/cli/deploy/run.rs`.

- [ ] **Step 1: Find the deploy write phase**

Open `src/cli/deploy/run.rs`. Locate where the deploy actually starts mutating the target — the call into `cli::deploy::apply::run` (or wherever the per-kind PATCH/POST/DELETE work begins). The exact spot may vary; the lock should bracket the create + apply phases.

- [ ] **Step 2: Acquire the target lock before the write phase**

Just before the create-sweep / apply-sweep block (which is typically inside the user-confirmed `if proceed` branch), add:

```rust
let tgt_paths = crate::paths::Paths::for_env(&cwd, tgt);
let _tgt_lock = crate::cli::sync::lock::EnvLock::acquire(
    &tgt_paths.env_lock(),
    std::time::Duration::from_secs(30),
)?;
```

Place the variable in scope for the entire write phase. The lock releases when `_tgt_lock` drops (end of scope).

- [ ] **Step 3: Add an integration test**

In `tests/cli_deploy.rs`, add:

```rust
#[tokio::test]
async fn deploy_waits_for_tgt_env_lock() {
    use rdc::cli::sync::lock::EnvLock;
    use std::time::Duration;

    // Set up a tiny deploy fixture: src env "test", tgt env "prod", both
    // empty. Use the existing helpers in tests/cli_deploy.rs.
    let (project, _src_server, _tgt_server) = setup_empty_two_env_project().await;

    // Hold the prod lock for 600 ms.
    let prod_lock_path = project.path().join(".rdc/state/prod.lock");
    std::fs::create_dir_all(prod_lock_path.parent().unwrap()).unwrap();
    let blocker = std::thread::spawn(move || {
        let lock = EnvLock::acquire(&prod_lock_path, Duration::from_secs(2)).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        drop(lock);
    });

    let start = std::time::Instant::now();
    let cmd = std::process::Command::new(env!("CARGO_BIN_EXE_rdc"))
        .args(["deploy", "test", "prod", "--yes"])
        .current_dir(project.path())
        .output()
        .unwrap();
    let elapsed = start.elapsed();

    blocker.join().unwrap();
    assert!(cmd.status.success(), "deploy failed: {}", String::from_utf8_lossy(&cmd.stderr));
    assert!(
        elapsed >= Duration::from_millis(500),
        "deploy returned too quickly ({elapsed:?}); should have waited for the lock"
    );
}
```

If `setup_empty_two_env_project` doesn't exist, look at the existing tests in `tests/cli_deploy.rs` and adapt their setup pattern. The key assertion is the wait time.

- [ ] **Step 4: Run the suite**

Run: `cargo test -p rdc --test cli_deploy deploy_waits_for_tgt_env_lock -- --nocapture`
Expected: PASS — deploy waits, succeeds.

Run: `cargo test -p rdc` — full suite still green.

- [ ] **Step 5: Stage**

```bash
git add src/cli/deploy/run.rs tests/cli_deploy.rs
```

---

## Task 4: Add `--watch` / `--poll-interval` / `--no-poll` / `-v` flags

**Goal:** Extend the `Sync` clap variant. Reject incompatible flag combinations at parse time. Route `--watch` to a stub `cli::sync::watch::run_watch` (a real implementation lands in Task 6+).

**Files:**
- Modify: `src/cli/mod.rs`.
- Modify: `src/cli/sync/mod.rs` (add `pub mod watch;`).
- Create: `src/cli/sync/watch.rs` (stub for now).

- [ ] **Step 1: Add the flags**

In `src/cli/mod.rs`, find the `Sync` variant (added in earlier work). Extend it:

```rust
Sync {
    env: Option<String>,
    #[arg(long = "dry-run")]
    dry_run: bool,
    #[arg(long = "diff", requires = "dry_run")]
    diff: bool,
    #[arg(long = "allow-deletes")]
    allow_deletes: bool,
    #[arg(long = "no-push", conflicts_with = "no_pull")]
    no_push: bool,
    #[arg(long = "no-pull", conflicts_with = "no_push")]
    no_pull: bool,
    /// Watch local files + poll the env continuously; reconcile on each event.
    #[arg(long = "watch", conflicts_with_all = ["dry_run", "diff"])]
    watch: bool,
    /// Poll cadence for remote drift in watch mode. Accepts human durations
    /// (`30s`, `2m`, `5m`). Default `60s`.
    #[arg(long = "poll-interval", value_name = "DURATION", default_value = "60s", requires = "watch")]
    poll_interval: String,
    /// Disable remote polling in watch mode. Outbound (file-event) sync stays.
    #[arg(long = "no-poll", requires = "watch", conflicts_with = "poll_interval")]
    no_poll: bool,
    /// Print every cycle in watch mode, including no-op cycles.
    #[arg(short = 'v', long = "verbose", requires = "watch")]
    verbose: bool,
},
```

- [ ] **Step 2: Add the routing**

In `src/cli/mod.rs::run`, update the `Sync` arm:

```rust
Some(Command::Sync { env, dry_run, diff, allow_deletes, no_push, no_pull, watch, poll_interval, no_poll, verbose }) => {
    let env = crate::cli::env_picker::pick_env("Which env to sync?", env)?;
    let interactive = crate::cli::resolve::is_interactive(cli.yes);
    if watch {
        let poll = if no_poll {
            None
        } else {
            Some(parse_duration(&poll_interval)?)
        };
        with_401_retry(&env, || {
            crate::cli::sync::watch::run_watch(
                &env, interactive, allow_deletes, no_push, no_pull, poll, verbose,
            )
        })
        .await
    } else {
        with_401_retry(&env, || {
            crate::cli::sync::run(&env, interactive, dry_run, diff, allow_deletes, no_push, no_pull)
        })
        .await
    }
}
```

Add a small `parse_duration` helper at the bottom of `src/cli/mod.rs`:

```rust
fn parse_duration(s: &str) -> anyhow::Result<std::time::Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(s.len())
    );
    let n: u64 = num.parse().map_err(|_| anyhow::anyhow!("invalid duration '{s}'; expected forms like '30s', '2m', '5m'"))?;
    match unit {
        "s" | "" => Ok(std::time::Duration::from_secs(n)),
        "m" => Ok(std::time::Duration::from_secs(n * 60)),
        "h" => Ok(std::time::Duration::from_secs(n * 3600)),
        _ => anyhow::bail!("invalid duration unit '{unit}'; use s / m / h"),
    }
}
```

- [ ] **Step 3: Create the stub `watch::run_watch`**

Create `src/cli/sync/watch.rs`:

```rust
//! `rdc sync --watch <env>` — foreground watch mode.
//!
//! Spec: docs/superpowers/specs/2026-05-14-watch-mode-design.md

use anyhow::Result;
use std::time::Duration;

pub async fn run_watch(
    _env: &str,
    _interactive: bool,
    _allow_deletes: bool,
    _no_push: bool,
    _no_pull: bool,
    _poll_interval: Option<Duration>,
    _verbose: bool,
) -> Result<()> {
    anyhow::bail!("watch mode not yet implemented");
}
```

In `src/cli/sync/mod.rs`, add `pub mod watch;` near the other `pub mod` lines.

- [ ] **Step 4: Verify help text + parse rejections**

```bash
cargo run -- sync --help
```

Confirm `--watch`, `--poll-interval`, `--no-poll`, `-v` appear.

```bash
cargo run -- sync test --watch --dry-run
```

Expected: error mentioning `--watch` cannot be combined with `--dry-run`.

```bash
cargo run -- sync test --poll-interval 30s
```

Expected: error — `--poll-interval` requires `--watch`.

- [ ] **Step 5: Add a clap parse test**

In an existing test file (e.g., `tests/cli_misc.rs` or wherever clap tests live), add:

```rust
#[test]
fn sync_watch_and_dry_run_are_mutually_exclusive() {
    use clap::Parser;
    let result = rdc::cli::Cli::try_parse_from(["rdc", "sync", "test", "--watch", "--dry-run"]);
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("--watch") || err.contains("--dry-run"), "{err}");
}

#[test]
fn sync_poll_interval_parses_human_duration() {
    use clap::Parser;
    let cli = rdc::cli::Cli::try_parse_from(["rdc", "sync", "test", "--watch", "--poll-interval", "30s"])
        .expect("valid CLI");
    if let Some(rdc::cli::Command::Sync { poll_interval, .. }) = cli.command {
        assert_eq!(poll_interval, "30s");
    } else {
        panic!("expected Sync variant");
    }
}
```

- [ ] **Step 6: Run the suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS — new tests green, no regressions.

- [ ] **Step 7: Stage**

```bash
git add Cargo.toml src/cli/mod.rs src/cli/sync/mod.rs src/cli/sync/watch.rs tests/cli_misc.rs
```

---

## Task 5: Add `notify` dependency + ctrl-c handler

**Goal:** Wire the `notify` crate dependency and tokio's `signal` feature. The watch stub now exits cleanly on ctrl-c without doing anything else.

**Files:**
- Modify: `Cargo.toml`.
- Modify: `src/cli/sync/watch.rs`.

- [ ] **Step 1: Add `notify` and the tokio `signal` feature**

In `Cargo.toml`'s `[dependencies]`:

```toml
notify = "6"
```

And update the tokio dependency line:

```toml
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
```

Run `cargo build` — should resolve.

- [ ] **Step 2: Implement the ctrl-c skeleton**

Replace `src/cli/sync/watch.rs` contents with:

```rust
//! `rdc sync --watch <env>` — foreground watch mode.
//!
//! Spec: docs/superpowers/specs/2026-05-14-watch-mode-design.md

use anyhow::Result;
use std::time::Duration;

pub async fn run_watch(
    env: &str,
    _interactive: bool,
    _allow_deletes: bool,
    _no_push: bool,
    _no_pull: bool,
    poll_interval: Option<Duration>,
    _verbose: bool,
) -> Result<()> {
    eprintln!("watching envs/{env}/ ...");
    if let Some(d) = poll_interval {
        eprintln!("polling {env} every {}s ...", d.as_secs());
    } else {
        eprintln!("polling disabled");
    }
    tokio::signal::ctrl_c().await?;
    eprintln!("\nstopping watch.");
    Ok(())
}
```

- [ ] **Step 3: Manual smoke test**

```bash
cargo run -- sync test --watch --no-poll
```

Confirm output:

```
watching envs/test/ ...
polling disabled
```

Then press Ctrl-C. Confirm:

```
^C
stopping watch.
```

Exit code 0. (This needs a working `test` env in the current directory or a `rdc.toml`. If absent, test against a freshly-`rdc init`-ed scratch directory, or accept that this manual step is informational and skip it.)

- [ ] **Step 4: Run the suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS — no behavior changes affecting existing tests.

- [ ] **Step 5: Stage**

```bash
git add Cargo.toml src/cli/sync/watch.rs
```

---

## Task 6: Initial reconcile on watch start

**Goal:** Before the watch loop, run one full `sync::run_cycle` to bring the env to a known state.

**Files:**
- Modify: `src/cli/sync/watch.rs`.

- [ ] **Step 1: Update `run_watch` to call `run_cycle`**

Replace the body of `run_watch` in `src/cli/sync/watch.rs` (keeping the same signature):

```rust
pub async fn run_watch(
    env: &str,
    interactive: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    poll_interval: Option<Duration>,
    _verbose: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let paths = crate::paths::Paths::for_env(&cwd, env);

    // Initial reconcile.
    {
        let _lock = crate::cli::sync::lock::EnvLock::acquire(
            &paths.env_lock(),
            Duration::from_secs(30),
        )?;
        crate::cli::sync::run_cycle(
            env, interactive, false, false, allow_deletes, no_push, no_pull, true,
        ).await?;
    }

    eprintln!("watching envs/{env}/ ...");
    if let Some(d) = poll_interval {
        eprintln!("polling {env} every {}s ...", d.as_secs());
    } else {
        eprintln!("polling disabled");
    }

    tokio::signal::ctrl_c().await?;
    eprintln!("\nstopping watch.");
    Ok(())
}
```

The initial reconcile uses `auto_confirm_non_destructive = true` (the watch-mode default) since the user committing to `--watch` is implicitly opting into auto-apply.

- [ ] **Step 2: Add an integration test**

In `tests/cli_sync.rs`, add (use the existing `MockServer` + `init_test_project` patterns):

```rust
#[tokio::test]
async fn sync_watch_initial_reconcile_pulls_remote_creates() {
    let server = MockServer::start().await;
    mount_empty_listings(&server).await;
    mount_label_listing(&server, &[label_fixture("audit-hold", 1)]).await;

    let project = init_test_project(&server, "dev").await;
    let path = project.path().to_path_buf();

    let cwd_lock = cwd_lock();
    let _guard = cwd_lock.lock().unwrap();
    let _saved = restore_cwd_on_drop(std::env::current_dir().unwrap());
    std::env::set_current_dir(&path).unwrap();

    // Spawn run_watch; cancel it after a short delay.
    let watch_handle = tokio::spawn(async {
        let result = rdc::cli::sync::watch::run_watch(
            "dev", false, false, false, false, None, false,
        ).await;
        // Watch will block on ctrl_c — we abort the task externally below.
        result
    });

    // Give the initial reconcile time to complete + reach the ctrl_c await.
    tokio::time::sleep(Duration::from_millis(800)).await;
    watch_handle.abort();
    let _ = watch_handle.await; // ignore abort error

    // Assert: the label file was pulled by the initial reconcile.
    let label_path = path.join("envs/dev/labels/audit-hold.json");
    assert!(label_path.exists(), "initial reconcile should have pulled the label");
}
```

This test exercises the initial-reconcile-then-block path. The `abort` is the test's stand-in for ctrl-c.

(The exact helper names — `mount_empty_listings`, `mount_label_listing`, `label_fixture`, `init_test_project`, `cwd_lock`, `restore_cwd_on_drop` — should match what's already in `tests/cli_sync.rs`. Read the file for the actual names.)

- [ ] **Step 3: Run**

Run: `cargo test -p rdc --test cli_sync sync_watch_initial_reconcile_pulls_remote_creates -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Stage**

```bash
git add src/cli/sync/watch.rs tests/cli_sync.rs
```

---

## Task 7: `CycleTrigger` channel + extracted `event_loop`

**Goal:** Internal architecture for the watch loop — a single `mpsc::channel<CycleTrigger>` consumed by a testable `event_loop`. No real triggers wired yet; the loop just waits for events.

**Files:**
- Modify: `src/cli/sync/watch.rs`.

- [ ] **Step 1: Define the trigger enum and the event_loop**

Add to `src/cli/sync/watch.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CycleTrigger {
    /// A local file changed (after debounce).
    FileEvent,
    /// The poll timer fired.
    Poll,
}

/// The testable inner loop: drain events, run cycles, exit on shutdown.
/// Tests call this directly with synthetic channels.
pub(crate) async fn event_loop(
    env: &str,
    interactive: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    verbose: bool,
    mut events: tokio::sync::mpsc::Receiver<CycleTrigger>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let paths = crate::paths::Paths::for_env(&cwd, env);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            evt = events.recv() => {
                let Some(trigger) = evt else { break };
                // Coalesce: drain any other pending events with try_recv.
                while events.try_recv().is_ok() {}

                let cycle_started = std::time::Instant::now();
                let _lock = crate::cli::sync::lock::EnvLock::acquire(
                    &paths.env_lock(),
                    std::time::Duration::from_secs(30),
                )?;
                let outcome = crate::cli::sync::run_cycle(
                    env, interactive, false, false, allow_deletes, no_push, no_pull, true,
                ).await?;
                drop(_lock);

                let elapsed = cycle_started.elapsed();
                print_cycle_summary(trigger, &outcome, elapsed, verbose);
            }
        }
    }
    Ok(())
}

fn print_cycle_summary(
    trigger: CycleTrigger,
    outcome: &crate::cli::sync::CycleOutcome,
    elapsed: std::time::Duration,
    verbose: bool,
) {
    let total = outcome.items_pushed + outcome.items_pulled + outcome.conflicts + outcome.remote_deletes_resolved;
    if total == 0 && !verbose {
        return; // quiet by default
    }
    let now = chrono::Local::now().format("%H:%M:%S");
    let dir = if outcome.items_pulled > 0 && outcome.items_pushed == 0 {
        "←"
    } else if outcome.items_pushed > 0 && outcome.items_pulled == 0 {
        "→"
    } else {
        "↔"
    };
    let kind = match trigger {
        CycleTrigger::FileEvent => "file",
        CycleTrigger::Poll => "poll",
    };
    let _ = kind; // currently unused in output; kept for verbose enrichment later
    if total == 0 {
        eprintln!("[{now}] (idle)");
    } else {
        eprintln!(
            "[{now}] {dir} cycle: pushed {}, pulled {}, conflicts {}, deletes {} ({:.1}s)",
            outcome.items_pushed,
            outcome.items_pulled,
            outcome.conflicts,
            outcome.remote_deletes_resolved,
            elapsed.as_secs_f32()
        );
    }
}
```

Note: the formatter uses `chrono::Local`. Check if `chrono` is already a dependency. If not, this task adds it. Alternative: use `std::time::SystemTime` and manually format. **Simpler — use `chrono`** (likely already a dep due to model `modified_at` parsing; verify in `Cargo.toml`).

If `chrono` is missing, prefer `time` crate (already in tokio's transitive deps) or a tiny manual formatter:

```rust
fn now_hhmmss() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_today = secs % 86400;
    let h = secs_today / 3600;
    let m = (secs_today % 3600) / 60;
    let s = secs_today % 60;
    format!("{h:02}:{m:02}:{s:02}")
}
```

This is UTC, not local; document it. Acceptable for v1.

- [ ] **Step 2: Update `run_watch` to spawn the inner loop**

Replace the body of `run_watch` (after the initial reconcile) with:

```rust
    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    // Wire ctrl-c → shutdown.
    let _shutdown_handle = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = shutdown_tx.send(());
    });

    // TODO Task 8/9: wire file watcher + poll timer into events_tx.
    let _ = events_tx; // suppress unused for now
    let _ = poll_interval; // suppress unused for now

    event_loop(env, interactive, allow_deletes, no_push, no_pull, verbose, events_rx, shutdown_rx).await?;
    eprintln!("\nstopping watch.");
    Ok(())
```

- [ ] **Step 3: Add a unit test for the event_loop**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};

    #[tokio::test]
    async fn event_loop_exits_cleanly_on_shutdown() {
        let (_tx, rx) = mpsc::channel::<CycleTrigger>(8);
        let (sh_tx, sh_rx) = oneshot::channel();

        // The event_loop expects to be able to load a project — without one,
        // the lock acquire path fails on a non-existent .rdc/state/ dir.
        // For this test, we just shut down immediately.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".rdc/state")).unwrap();
        let saved_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        sh_tx.send(()).unwrap(); // shutdown before any event
        let result = event_loop("test", false, false, false, false, false, rx, sh_rx).await;

        std::env::set_current_dir(saved_cwd).unwrap();
        assert!(result.is_ok(), "{result:?}");
    }
}
```

(Note: this test relies on the cwd discipline that other tests use. If the test file already has a `cwd_lock` helper, use it. Otherwise this test is fragile under parallel test execution — flag as such with `#[serial]` if `serial_test` is available; otherwise accept the flakiness for unit tests of this kind.)

- [ ] **Step 4: Run**

Run: `cargo test -p rdc --lib cli::sync::watch::tests -- --nocapture`
Expected: PASS.

Run: `cargo test -p rdc -- --nocapture` — full suite green.

- [ ] **Step 5: Stage**

```bash
git add src/cli/sync/watch.rs
```

---

## Task 8: Poll timer

**Goal:** A `tokio::time::interval` spawn that sends `CycleTrigger::Poll` to `events_tx` every `<poll-interval>` seconds. Disabled by `--no-poll`.

**Files:**
- Modify: `src/cli/sync/watch.rs`.

- [ ] **Step 1: Spawn the poll task in `run_watch`**

In `run_watch`, after the ctrl-c spawn and before the `event_loop` call:

```rust
    if let Some(interval_duration) = poll_interval {
        let tx = events_tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval_duration);
            // skip the immediate first tick — initial reconcile already ran
            tick.tick().await;
            loop {
                tick.tick().await;
                if tx.send(CycleTrigger::Poll).await.is_err() {
                    break;
                }
            }
        });
    }
```

- [ ] **Step 2: Write a unit test that exercises the poll interval**

```rust
#[tokio::test(start_paused = true)]
async fn poll_interval_produces_one_event_per_tick() {
    use tokio::sync::mpsc;
    use std::time::Duration;

    let (tx, mut rx) = mpsc::channel::<CycleTrigger>(8);
    let interval = Duration::from_secs(60);
    let _h = tokio::spawn(async move {
        let mut t = tokio::time::interval(interval);
        t.tick().await; // skip first
        loop {
            t.tick().await;
            if tx.send(CycleTrigger::Poll).await.is_err() { break; }
        }
    });

    // Advance time by 70 s — should produce exactly one Poll.
    tokio::time::advance(Duration::from_secs(70)).await;
    let evt = rx.recv().await.unwrap();
    assert_eq!(evt, CycleTrigger::Poll);
    assert!(rx.try_recv().is_err(), "second event arrived too soon");

    // Advance another 60 s — second Poll.
    tokio::time::advance(Duration::from_secs(60)).await;
    let evt = rx.recv().await.unwrap();
    assert_eq!(evt, CycleTrigger::Poll);
}
```

- [ ] **Step 3: Run**

Run: `cargo test -p rdc --lib cli::sync::watch::tests::poll_interval_produces_one_event_per_tick -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Stage**

```bash
git add src/cli/sync/watch.rs
```

---

## Task 9: File watcher via `notify`

**Goal:** Set up a `notify` recursive watcher on `envs/<env>/`. Each event sends `CycleTrigger::FileEvent` to `events_tx`. Filter out shadow files (`.<env>`, `.<env>-deleted`) and the `.rdc/` subtree.

**Files:**
- Modify: `src/cli/sync/watch.rs`.

- [ ] **Step 1: Add the file-watcher setup function**

```rust
fn spawn_file_watcher(
    env: String,
    env_root: std::path::PathBuf,
    tx: tokio::sync::mpsc::Sender<CycleTrigger>,
) -> Result<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};

    let event_handler = move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else { return; };
        // Filter: ignore .rdc/ and shadow artifacts.
        for path in &event.paths {
            if path_should_be_ignored(path, &env) {
                continue;
            }
            let _ = tx.blocking_send(CycleTrigger::FileEvent);
            return;
        }
    };

    let mut watcher = notify::recommended_watcher(event_handler)?;
    watcher.watch(&env_root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

fn path_should_be_ignored(path: &std::path::Path, env: &str) -> bool {
    // Ignore .rdc/ subtree — daemon-managed.
    if path.components().any(|c| c.as_os_str() == ".rdc") {
        return true;
    }
    // Ignore shadow artifacts.
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    crate::paths::is_shadow_artifact(name, env)
}
```

- [ ] **Step 2: Wire it in `run_watch`**

After the poll-timer block:

```rust
    let env_root = paths.env_root();
    let _watcher = spawn_file_watcher(env.to_string(), env_root, events_tx.clone())?;
```

Note: `_watcher` is bound so it stays alive for the duration of `run_watch`. Dropping a `RecommendedWatcher` stops watching.

- [ ] **Step 3: Add a unit test**

Real file events are flaky in tests. Instead, test the filter:

```rust
#[test]
fn path_should_be_ignored_rejects_rdc_subtree() {
    assert!(path_should_be_ignored(std::path::Path::new("/proj/.rdc/state/test.lock.json"), "test"));
}

#[test]
fn path_should_be_ignored_rejects_shadow_files() {
    assert!(path_should_be_ignored(std::path::Path::new("/proj/envs/test/labels/a.json.test"), "test"));
    assert!(path_should_be_ignored(std::path::Path::new("/proj/envs/test/labels/a.json.test-deleted"), "test"));
}

#[test]
fn path_should_be_ignored_accepts_normal_files() {
    assert!(!path_should_be_ignored(std::path::Path::new("/proj/envs/test/labels/a.json"), "test"));
    assert!(!path_should_be_ignored(std::path::Path::new("/proj/envs/test/overlay.toml"), "test"));
}
```

- [ ] **Step 4: Run**

Run: `cargo test -p rdc --lib cli::sync::watch::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Stage**

```bash
git add src/cli/sync/watch.rs
```

---

## Task 10: Debounce file events

**Goal:** Multiple file events within 500 ms coalesce into one cycle. Implemented inside `event_loop` via a sleep-then-drain after the first event.

**Files:**
- Modify: `src/cli/sync/watch.rs`.

- [ ] **Step 1: Update `event_loop` to debounce file events**

Inside the `events.recv()` arm of `event_loop`'s `select!`:

```rust
                let Some(trigger) = evt else { break };
                // Debounce only file events. Poll events run immediately.
                if matches!(trigger, CycleTrigger::FileEvent) {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                // Coalesce any pending events that arrived during the debounce.
                while events.try_recv().is_ok() {}
                // ... existing cycle execution ...
```

- [ ] **Step 2: Add a unit test**

```rust
#[tokio::test(start_paused = true)]
async fn debounce_coalesces_burst_of_file_events() {
    use tokio::sync::{mpsc, oneshot};

    let (tx, rx) = mpsc::channel::<CycleTrigger>(16);
    let (sh_tx, sh_rx) = oneshot::channel();

    // Send 5 file events in a tight burst.
    for _ in 0..5 {
        tx.send(CycleTrigger::FileEvent).await.unwrap();
    }

    // Spawn the loop with a project context (similar setup to event_loop_exits_cleanly).
    // Use a separate temp dir; the cycle will fail because there's no rdc.toml,
    // so wrap the event_loop call in a way that surfaces the call count rather
    // than the cycle outcome. ALTERNATIVE: this test is hard to write without a
    // mock for run_cycle. Replace with a simpler unit that exercises the
    // drain logic in isolation:

    let mut count = 0;
    let _ = tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    // We started with 5; after a single "first event consumed" + drain, the
    // remaining 4 are drained. We expect count == 4 since the burst was
    // emitted before the drain.
    // Specifically: this test doesn't exercise event_loop directly; it
    // demonstrates the drain logic. Adjust below to fit the actual scenario.

    let _ = sh_tx; let _ = sh_rx;
    let _ = count; // documentation
}
```

The above test is illustrative. The real assertion to make is **end-to-end**: "5 file events in 100ms → exactly 1 cycle." That requires intercepting `run_cycle`. Since `run_cycle` is real work, the practical test is the integration version in Task 11's combined test or the existing `sync_watch_initial_reconcile_pulls_remote_creates` extension.

For this task, the **unit test** scope is: assert that the debounce sleep + drain produces the right call count. Add a focused helper that doesn't run a real cycle:

```rust
async fn drain_after_debounce<T>(rx: &mut tokio::sync::mpsc::Receiver<T>) -> usize {
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let mut drained = 0;
    while rx.try_recv().is_ok() {
        drained += 1;
    }
    drained
}

#[tokio::test(start_paused = true)]
async fn debounce_then_drain_coalesces_burst() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CycleTrigger>(16);
    for _ in 0..5 {
        tx.send(CycleTrigger::FileEvent).await.unwrap();
    }
    // Consume the first event (caller would have done this with rx.recv()).
    let _ = rx.recv().await.unwrap();
    let extras = drain_after_debounce(&mut rx).await;
    assert_eq!(extras, 4, "expected 4 extra events drained after debounce");
}
```

This narrowly tests the coalesce logic. The end-to-end watch behavior is covered in later integration tests.

- [ ] **Step 3: Run**

Run: `cargo test -p rdc --lib cli::sync::watch::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Stage**

```bash
git add src/cli/sync/watch.rs
```

---

## Task 11: Watcher pause/resume around cycles (feedback-loop prevention)

**Goal:** Cycle's own writes mustn't trigger another cycle. Pause the `notify` watcher before each cycle; resume after.

**Files:**
- Modify: `src/cli/sync/watch.rs`.

- [ ] **Step 1: Refactor `event_loop` to take watcher control**

`event_loop` needs a way to pause/resume the watcher. Pass an `Arc<Mutex<Option<notify::RecommendedWatcher>>>` so the loop can `take()` it to pause and reinstall it to resume. Or simpler: pass two closures `pause: impl Fn()` and `resume: impl Fn()`.

Simplest implementation: pass the `env_root` path and the watcher itself:

```rust
pub(crate) async fn event_loop(
    env: &str,
    interactive: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    verbose: bool,
    mut events: tokio::sync::mpsc::Receiver<CycleTrigger>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
    mut watcher: Option<notify::RecommendedWatcher>,
    env_root: std::path::PathBuf,
) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    // ...
}
```

Inside the cycle execution block:

```rust
                // Pause the watcher around our own writes to avoid feedback loops.
                if let Some(w) = watcher.as_mut() {
                    let _ = w.unwatch(&env_root);
                }

                let cycle_started = std::time::Instant::now();
                let _lock = crate::cli::sync::lock::EnvLock::acquire(
                    &paths.env_lock(),
                    std::time::Duration::from_secs(30),
                )?;
                let outcome = crate::cli::sync::run_cycle(
                    env, interactive, false, false, allow_deletes, no_push, no_pull, true,
                ).await?;
                drop(_lock);

                // Resume watching. Drop any events that arrived during pause.
                if let Some(w) = watcher.as_mut() {
                    let _ = w.watch(&env_root, RecursiveMode::Recursive);
                }
                // Drain stale events from the pause window.
                while events.try_recv().is_ok() {}
```

Update `run_watch` to pass `Some(watcher)` and `env_root`.

For tests where there's no real watcher, pass `None`.

- [ ] **Step 2: Add a test stub**

Real test is integration-level (see Task 15). For this task, ensure the unit tests still pass with the new signature; update the existing `event_loop_exits_cleanly_on_shutdown` to pass `None, env_root`:

```rust
let result = event_loop(
    "test", false, false, false, false, false, rx, sh_rx, None, std::path::PathBuf::new(),
).await;
```

- [ ] **Step 3: Run**

Run: `cargo test -p rdc --lib cli::sync::watch::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Stage**

```bash
git add src/cli/sync/watch.rs
```

---

## Task 12: Wire `run_cycle` outcome counters

**Goal:** Populate `CycleOutcome` so the watch summary line is meaningful. Today `run_cycle` returns `CycleOutcome::default()`.

**Files:**
- Modify: `src/cli/sync/mod.rs`.
- Modify: `src/cli/sync/execute.rs`.

- [ ] **Step 1: Have `execute::run` return counts**

`execute::run` already touches every item; have it accumulate. Change its return type:

```rust
pub async fn run(
    ctx: &mut crate::cli::pull::common::PullCtx<'_>,
    catalog: &crate::cli::pull::common::RemoteCatalog,
    classified: &[ClassifiedItem],
    no_push: bool,
    no_pull: bool,
    interactive: bool,
    progress: &std::sync::Arc<crate::progress::OverallProgress>,
) -> anyhow::Result<crate::cli::sync::CycleOutcome> {
    let mut outcome = crate::cli::sync::CycleOutcome::default();
    // ... existing body, incrementing fields as work happens ...
    Ok(outcome)
}
```

Increment `outcome.items_pushed` for each LocalEdit/LocalCreate dispatched, `items_pulled` for each RemoteEdit/RemoteCreate, `conflicts` for each BothDiverged / LocalEditRemoteDelete / LocalDeleteRemoteEdit, `remote_deletes_resolved` for each RemoteDelete handled (resolved by user, not skipped). Use the existing classification as the source of counts.

- [ ] **Step 2: Update `sync::run_cycle` to return the outcome**

At the end of `run_cycle`, replace `Ok(CycleOutcome::default())` with `Ok(outcome)` where `outcome` is the result of `execute::run`.

- [ ] **Step 3: Run the suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS — existing tests assume nothing about return values; the new return type plumbing should be transparent.

If any test asserts on the printed summary, update those assertions.

- [ ] **Step 4: Stage**

```bash
git add src/cli/sync/mod.rs src/cli/sync/execute.rs
```

---

## Task 13: Auto-confirm non-destructive cycles

**Goal:** Verify that the `auto_confirm_non_destructive = true` path in `run_cycle` (added in Task 2) actually suppresses the plan prompt in watch cycles. Add a focused test.

**Files:**
- Modify: `tests/cli_sync.rs`.

- [ ] **Step 1: Add an integration test**

```rust
#[tokio::test]
async fn sync_watch_non_destructive_cycle_skips_confirm() {
    // Setup: env with one remote-only label (RemoteCreate, non-destructive).
    // Spawn run_watch with interactive=true (sim TTY); capture stderr.
    // Abort after 800ms.
    // Assert: stderr contains the cycle outcome line but NOT "Proceed? [y/N]".

    let server = MockServer::start().await;
    mount_empty_listings(&server).await;
    mount_label_listing(&server, &[label_fixture("audit-hold", 1)]).await;

    let project = init_test_project(&server, "dev").await;
    let path = project.path().to_path_buf();
    let cwd_lock_g = cwd_lock().lock().unwrap();
    let _saved = restore_cwd_on_drop(std::env::current_dir().unwrap());
    std::env::set_current_dir(&path).unwrap();

    // capture stderr by redirecting to a file (since the prompt would normally
    // be on stderr). Use the project dir for the redirect.
    // ...this is OS-specific. SIMPLER ALTERNATIVE:

    // Spawn run_watch in-process. The prompt would block on stdin — if it
    // blocks, the test times out. We assert NON-blocking behavior by passing
    // interactive=true and verifying the call returns within a short window.
    let handle = tokio::spawn(async {
        rdc::cli::sync::watch::run_watch("dev", true, false, false, false, None, false).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    handle.abort();

    let label_path = path.join("envs/dev/labels/audit-hold.json");
    assert!(label_path.exists(), "label should have been pulled; if it didn't, the cycle was blocked on a prompt");

    drop(cwd_lock_g);
}
```

- [ ] **Step 2: Run**

Run: `cargo test -p rdc --test cli_sync sync_watch_non_destructive_cycle_skips_confirm -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Stage**

```bash
git add tests/cli_sync.rs
```

---

## Task 14: Error handling — network retry, 401, parse errors

**Goal:** In watch mode, cycle errors don't terminate the daemon. Specific failure modes:
- **Network 5xx / timeout** during list_remote or push: log a one-line warning, continue.
- **401 Unauthorized**: pause and prompt for a new token inline (reuse `with_401_retry` shape).
- **Local file parse error** (malformed JSON the user just saved): log `[HH:MM] envs/test/<path>: invalid JSON, skipping`. Continue.

**Files:**
- Modify: `src/cli/sync/watch.rs`.

- [ ] **Step 1: Wrap the cycle call in a non-fatal handler**

In `event_loop`, change:

```rust
let outcome = crate::cli::sync::run_cycle(...).await?;
```

To:

```rust
let outcome = match crate::cli::sync::run_cycle(...).await {
    Ok(o) => o,
    Err(e) if crate::api::anyhow_has_status(&e, 401) => {
        // Prompt for a new token inline; retry the cycle once.
        eprintln!("[{}] auth expired", now_hhmmss());
        crate::cli::auth::refresh_token_interactively(env).await?;
        crate::cli::sync::run_cycle(env, interactive, false, false, allow_deletes, no_push, no_pull, true).await?
    }
    Err(e) if is_transient_network_error(&e) => {
        eprintln!("[{}] cycle failed (transient): {e:#}", now_hhmmss());
        continue;
    }
    Err(e) if is_local_parse_error(&e) => {
        eprintln!("[{}] cycle failed (local file error): {e:#}", now_hhmmss());
        continue;
    }
    Err(e) => return Err(e),
};
```

Implement the predicates:

```rust
fn is_transient_network_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.to_string().contains("timed out")
            || c.to_string().contains("connection refused")
            || c.to_string().contains("5xx")
    })
}

fn is_local_parse_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string();
        s.contains("invalid JSON") || s.contains("serde_json")
    })
}
```

These are heuristic. Refine if integration tests show false positives.

- [ ] **Step 2: Add a unit test for the predicates**

```rust
#[test]
fn transient_network_error_recognizes_timeout() {
    let e = anyhow::anyhow!("listing labels for env 'test': connection timed out");
    assert!(is_transient_network_error(&e));
}

#[test]
fn parse_error_recognizes_invalid_json() {
    let e = anyhow::anyhow!("reading envs/test/labels/a.json: invalid JSON at line 3");
    assert!(is_local_parse_error(&e));
}
```

- [ ] **Step 3: Run**

Run: `cargo test -p rdc --lib cli::sync::watch::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Stage**

```bash
git add src/cli/sync/watch.rs
```

---

## Task 15: Integration tests for the full watch flow

**Goal:** A small suite of integration tests covering: initial reconcile, poll-driven cycle, file-event-driven cycle (via synthetic channel injection), and lock contention.

**Files:**
- Modify: `tests/cli_sync.rs`.

- [ ] **Step 1: Add `sync_watch_poll_catches_remote_drift`**

```rust
#[tokio::test]
async fn sync_watch_poll_catches_remote_drift() {
    let server = MockServer::start().await;
    mount_empty_listings(&server).await;
    // Initially mount one label.
    let labels_mock = mount_label_listing_mutable(&server, &[label_fixture("audit-hold", 1)]).await;

    let project = init_test_project(&server, "dev").await;
    let path = project.path().to_path_buf();
    let cwd_lock_g = cwd_lock().lock().unwrap();
    let _saved = restore_cwd_on_drop(std::env::current_dir().unwrap());
    std::env::set_current_dir(&path).unwrap();

    // Spawn watch with a 100ms poll cadence so the test runs fast.
    let handle = tokio::spawn(async move {
        rdc::cli::sync::watch::run_watch(
            "dev", false, false, false, false,
            Some(std::time::Duration::from_millis(100)),
            false,
        ).await
    });

    // Initial reconcile finishes (audit-hold is pulled).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(path.join("envs/dev/labels/audit-hold.json").exists());

    // Now change the remote.
    labels_mock.swap(&[label_fixture("audit-hold", 1), label_fixture("new-label", 2)]).await;

    // Wait one poll cycle + cycle execution time.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(path.join("envs/dev/labels/new-label.json").exists(),
        "poll should have picked up the new label");

    handle.abort();
    drop(cwd_lock_g);
}
```

`mount_label_listing_mutable` is a helper you'll add — it returns a struct with a `swap(new)` method. Implement it using wiremock's mock-modification primitives, or by resetting the server and remounting. If wiremock doesn't support hot-swapping easily, fall back to using a `tokio::sync::Mutex<Vec<Label>>` and a custom responder.

- [ ] **Step 2: Add `sync_watch_one_shot_blocks_on_lock`**

```rust
#[tokio::test]
async fn sync_watch_does_not_deadlock_with_one_shot_sync() {
    // Spawn watch; while it's idle, run a one-shot sync; assert both
    // complete without deadlock.
    let server = MockServer::start().await;
    mount_empty_listings(&server).await;

    let project = init_test_project(&server, "dev").await;
    let path = project.path().to_path_buf();
    let cwd_lock_g = cwd_lock().lock().unwrap();
    let _saved = restore_cwd_on_drop(std::env::current_dir().unwrap());
    std::env::set_current_dir(&path).unwrap();

    let watch_handle = tokio::spawn(async {
        rdc::cli::sync::watch::run_watch(
            "dev", false, false, false, false, None, false,
        ).await
    });

    // Give the watch's initial reconcile time to release the lock.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    // Run a one-shot sync — should complete without deadlock.
    let one_shot = rdc::cli::sync::run("dev", false, false, false, false, false, false).await;
    assert!(one_shot.is_ok(), "one-shot sync failed: {one_shot:?}");

    watch_handle.abort();
    drop(cwd_lock_g);
}
```

- [ ] **Step 3: Run**

Run: `cargo test -p rdc --test cli_sync sync_watch_ -- --nocapture`
Expected: PASS — both new tests, plus existing watch tests.

- [ ] **Step 4: Stage**

```bash
git add tests/cli_sync.rs
```

---

## Task 16: README update

**Goal:** Document `rdc sync --watch` in the README.

**Files:**
- Modify: `README.md`.

- [ ] **Step 1: Update the Commands table**

Find the `rdc sync <env>` row. Update its flag list to include `--watch [--poll-interval N] [--no-poll]`:

```markdown
| `rdc sync <env>` | Reconcile local snapshot and remote state. `--no-push` (audit), `--no-pull` (deploy from snapshot), `--watch` (foreground continuous sync). |
```

- [ ] **Step 2: Add a "Watch mode" subsection**

After the existing sync sections (and before `Promote test → prod`), add:

```markdown
## Watch mode

`rdc sync --watch <env>` runs a foreground continuous sync. Save a file in `envs/<env>/`; the daemon pushes the change within a second or two. Run `rdc sync` from another shell, or edit via the Rossum UI; the daemon pulls within the configured poll interval (default `60s`).

```sh
$ rdc sync --watch test
watching envs/test/ ...
polling test every 60s ...

[14:02:17] queues/cost-invoices                                → push (0.6s)
[14:05:41] labels/audit-hold (new)                             ← pull (0.4s)
[14:09:03] hooks/finance-totals — conflict
  local has changes:
    - threshold: 0.85
    + threshold: 0.95
  test has changes:
    - threshold: 0.85
    + threshold: 0.80
  [k] keep local  [r] use test  [e] edit  [s] skip  [a] abort > k
[14:09:21] hooks/finance-totals                                → push (0.4s)
```

Ctrl-C stops the watch. The daemon stays foreground — it doesn't fork; close the terminal tab to stop it. `--no-poll` disables remote polling (file events only). `--poll-interval 30s` tunes the cadence.

While the watch is running, you can still run `rdc sync test`, `rdc deploy test prod`, etc. from other shells — they coordinate via an advisory lock and wait briefly if the watch is mid-cycle.

Conflicts and destructive deletes prompt inline in the watch terminal. Non-destructive cycles auto-apply silently. `-v` prints every cycle including idle polls.
```

- [ ] **Step 3: Verify**

```bash
grep -n "rdc sync --watch" README.md
```

Expected: at least one match in the new subsection, one in the Commands table.

- [ ] **Step 4: Run the suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS — no source changes, so this is a sanity check.

- [ ] **Step 5: Stage**

```bash
git add README.md
```

---

## Spec Self-Review

Walk through `docs/superpowers/specs/2026-05-14-watch-mode-design.md` and confirm coverage:

- **CLI surface (§Design)**: Tasks 4 (`--watch`, `--poll-interval`, `--no-poll`, `-v`, flag conflicts).
- **Trigger model — file events**: Task 9 (`notify` + path filter).
- **Trigger model — poll**: Task 8 (`tokio::time::interval`).
- **Debounce**: Task 10.
- **Initial reconcile**: Task 6.
- **Watch cycle (auto-confirm non-destructive)**: Tasks 2 (`run_cycle` signature), 13 (test).
- **Conflict / destructive prompts inline**: covered transitively — existing resolver writes to stderr / reads from stdin; watch doesn't change that.
- **Advisory locks**: Tasks 1 (EnvLock), 2 (one-shot lock), 3 (deploy lock), 7 (watch lock per cycle).
- **Feedback-loop prevention**: Task 11.
- **Output format**: Tasks 7 (summary line), 12 (real counts).
- **Error UX**: Task 14.
- **Code shape (new modules)**: Tasks 1, 4-11.
- **Testing**: Tasks 1, 3, 6, 8, 10, 11, 13, 14, 15.
- **README**: Task 16.

If any spec requirement isn't pointed at, add a task. If the plan has placeholder text, fix it inline.
