# Desktop Two-Way Sync and Watch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the rdc desktop app `rdc sync <env>` and `rdc sync <env> --watch` with the semantics they have in the terminal — two-way reconciliation, a file watcher, a drift poll, and a real dialog for every prompt that would block a cycle.

**Architecture:** Three core capabilities land first, each invisible to the CLI: prompt output routed through `Log` so an embedder can see it; a thread-local prompt route so a blocked prompt can be answered by something that is not a terminal; and a parameterised watch loop with `run_watch` (CLI) and `embed::watch_logged` (app) as its two callers. Only then does the bridge grow `watch_env`/`stop_watch`/`answer_prompt` and the Flutter app grow the UI.

**Tech Stack:** Rust (`rdc` core, `rdc_bridge`), tokio (current-thread), `notify`, Flutter/Dart, flutter_rust_bridge 2.12.0.

**Spec:** `docs/superpowers/specs/2026-09-01-desktop-watch-and-promote-removal-design.md`

**Prerequisite plan:** `docs/superpowers/plans/2026-09-01-desktop-promote-removal.md` must be complete. Task 8 here deletes `embed::sync_push_logged`, which that plan leaves uncalled.

## Global Constraints

- **Never put customer names or customer-specific identifiers** — org/division/region codes, real environment names, queue/engine/hook slugs, hostnames, URLs, file paths — anywhere in this repository, including commit messages. Use `acme`, `main`, `invoices`, `dev`/`test`/`prod`.
- **Never `git push`.** Commit to local `main` only.
- **Work on `main`.** No `fix/` or `work/` branch.
- **Do not run repo-wide `cargo fmt`.** This repo is not fmt-clean under the local rustfmt; that failure is pre-existing.
- **rdc compiles slowly. Batch every edit in a task and compile once**, not once per TDD micro-step. Where a task's steps say "write the test, run it, implement, run it", that is still two compiles, not four — do not add more.
- **The CLI must stay byte-for-byte identical**, with exactly one deliberate exception: under `--watch`, a prompt now clears the in-place countdown line before drawing. Everything else — every prompt string, every diff body, every gate list — is unchanged. Task 1 exists to prove it.
- **flutter_rust_bridge is pinned at exactly 2.12.0.** `flutter_rust_bridge_codegen --version` must print `2.12.0` before regenerating.
- **The on-disk rdc contract is untouched.** `rdc.toml`, `secrets/`, `envs/`, `.rdc/state` keep their formats. A folder stays interchangeable between CLI and app.
- **The app's cycle flags are fixed: `interactive: true`, `allow_deletes: false`, `conflict_strategy: None`.** This combination is load-bearing (spec §4.3): `allow_deletes: true` would skip the gate entirely, `interactive: false` would `bail!` and kill the watch, and a `conflict_strategy` would resolve conflicts without asking. Do not "simplify" any of the three.
- Someone else may be working in this checkout. Never run bare `git stash` / `git checkout` / `git reset` / `git clean`. Only add the paths a task names.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/log.rs` | Adds `Log::writer()` → `LogWriter`, a `Write` that routes inline prompt output through the log's sink. Locks per write, never across the guard. |
| `src/cli/stdin_coord.rs` | Adds `Prompt` / `PromptKey` / `PromptKind` / `PromptRoute` / `install_route` / `announce`. Existing global-coordinator path unchanged. |
| `src/cli/resolve.rs` | Prompt sites announce what they are asking. Prompt text unchanged. |
| `src/cli/push/deletes.rs`, `push/mdh.rs`, `push/mdh_data.rs` | Four raw `eprint!` questions move onto the log writer and announce. |
| `src/cli/sync/execute.rs`, `src/cli/pull/common.rs` | Prompt output sink changes from `stderr().lock()` to `progress.writer()`. |
| `src/secrets.rs` | Adds `force_relogin` for a 401 with a clock-valid but revoked token. |
| `src/cli/sync/watch.rs` | `WatchConfig`, `CancelToken`, `TokenRefresher`; `event_loop` parameterised; `run_watch` becomes a thin CLI caller. |
| `src/cli/sync/embed.rs` | `sync_logged` (replaces the two logged wrappers) and `watch_logged`. |
| `desktop/rust/src/api/rdc.rs` | Two-way `sync_env`; `watch_env` / `stop_watch` / `answer_prompt`; the watch registry; the `PromptRoute` impl. |
| `desktop/rust/src/watch_registry.rs` | New. Global map of live watches: cancel token + answer sender, keyed by `(folder, env)`. Its own file so `rdc.rs` stays a thin FFI surface. |
| `desktop/lib/src/watch_state.dart` | New. Per-env watch state (`running`, `nextPollSecs`, `pendingPrompt`, `log`). Its own file so `app_state.dart` does not grow another concern. |
| `desktop/lib/src/app_state.dart` | Wires the watch stream into `WatchState`; owns the prompt queue. |
| `desktop/lib/src/dialogs.dart` | Adds `PromptDialog`. |
| `desktop/lib/src/home_page.dart` | Watch button, row action, badge, sidebar countdown. |
| `desktop/lib/src/settings.dart` | Adds the `watch` (per-env `pollSecs`) and `ackTwoWay` keys. |

---

### Task 1: Pin the CLI's prompt bytes before anything moves

**Files:**
- Modify: `src/cli/resolve.rs` (tests module only)

**Interfaces:**
- Consumes: nothing.
- Produces: three byte-exact regression tests that must still pass unchanged after Task 2 swaps the output sink. No production code changes.

Why first: the conflict, remote-delete and push-drift bodies are the real risk in Task 2. They go through `W: Write`, so a test can capture them today with the same `Cursor`/`Vec<u8>` harness the file already uses, and the identical assertion must hold once `W` becomes `LogWriter`.

- [ ] **Step 1: Add the three tests**

Append to `mod tests` in `src/cli/resolve.rs`:

```rust
    /// Byte-exact pin of the conflict prompt. Task 2 of the two-way-sync
    /// plan swaps this prompt's output sink from `stderr().lock()` to
    /// `Log::writer()`; the bytes must not move. If this test fails after
    /// that change, the CLI's output changed and the change is wrong.
    #[test]
    fn conflict_prompt_bytes_are_pinned() {
        use std::io::Cursor;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("queues/invoices.json");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, b"{\"name\":\"Invoices\"}").unwrap();

        let mut out: Vec<u8> = Vec::new();
        let _ = prompt_resolve_with_color(
            Cursor::new(b"s\n"),
            &mut out,
            1,
            1,
            ObjectRef { kind: "queues", slug: "invoices" },
            &local,
            b"{\"name\":\"Invoices EU\"}",
            "dev",
            ColorMode::Plain,
        )
        .unwrap();

        insta_like_pin("conflict", &String::from_utf8_lossy(&out));
    }

    #[test]
    fn remote_delete_prompt_bytes_are_pinned() {
        use std::io::Cursor;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("labels/audit-hold.json");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, b"{\"name\":\"Audit hold\"}").unwrap();

        let mut out: Vec<u8> = Vec::new();
        let _ = prompt_remote_delete_with_color(
            Cursor::new(b"s\n"),
            &mut out,
            ObjectRef { kind: "labels", slug: "audit-hold" },
            &local,
            "dev",
            ColorMode::Plain,
        )
        .unwrap();

        insta_like_pin("remote_delete", &String::from_utf8_lossy(&out));
    }

    /// Writes the captured text to `testdata/prompt_pins/<name>.txt` on
    /// first run and compares against it afterwards. Deliberately a plain
    /// file rather than a new dev-dependency: the point is a byte record
    /// that survives a refactor, and `git diff` on the file is the review.
    fn insta_like_pin(name: &str, actual: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/prompt_pins")
            .join(format!("{name}.txt"));
        if !path.exists() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, actual).unwrap();
            panic!("wrote a new pin at {}; re-run to verify it", path.display());
        }
        let expected = std::fs::read_to_string(&path).unwrap();
        pretty_assertions::assert_eq!(expected, actual, "prompt bytes moved: {name}");
    }
```

Check the exact parameter list of `prompt_remote_delete_with_color` before you compile — copy it from the existing `prompt_remote_delete_offers_restore_and_mirror_labels` test in the same module rather than trusting the snippet above, which shows the shape, not necessarily the arity.

- [ ] **Step 2: Run twice — once to write the pins, once to verify**

Run:
```bash
cargo test --lib cli::resolve::tests:: -- --nocapture 2>&1 | tail -20
```
Expected: FAIL with "wrote a new pin at …" for both.

Run it again.
Expected: both PASS.

- [ ] **Step 3: Read the pins**

Run:
```bash
cat testdata/prompt_pins/conflict.txt testdata/prompt_pins/remote_delete.txt
```
Expected: the change row, the `⌿`-style connector line with the file path, the diff body, and the `[k] keep local  [r] use dev  …` question. Confirm there is no customer-specific string in either file — they are committed, and `queues/invoices` / `labels/audit-hold` / `dev` are the neutral placeholders this repo requires.

- [ ] **Step 4: Commit**

```bash
git add src/cli/resolve.rs testdata/prompt_pins
git commit -m "test(cli): pin the conflict and remote-delete prompt bytes

These prompts are about to have their output sink swapped from raw stderr
to the Log's, so the app can see the diff. The bytes must not move; this
is what proves it.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `Log::writer()`, and route the three resolver sinks through it

**Files:**
- Modify: `src/log.rs`
- Modify: `src/cli/sync/execute.rs`
- Modify: `src/cli/pull/common.rs`

**Interfaces:**
- Consumes: Task 1's pins.
- Produces: `Log::writer(&self) -> LogWriter<'_>`, where `LogWriter: std::io::Write`. Callers pass `&mut progress.writer()` (or a `&mut dyn Write` reborrow) anywhere a `W: Write` prompt sink is wanted.

**The lock rule, and why:** `LogWriter` must acquire the log's mutex **per `write` call** and release it before returning. It must not hold the lock for the guard's lifetime. `resolve_conflicts` creates the sink once at the top and calls `progress.event(...)` many times while it is alive — a held lock would deadlock the first such call.

- [ ] **Step 1: Add `LogWriter` to `src/log.rs`**

After the `with_prompt` method, inside `impl Log`:

```rust
    /// A `Write` that routes inline prompt output — the conflict diff, the
    /// question line — through this log's sink instead of raw stderr.
    ///
    /// For `Log::new` the sink IS stderr, so terminal output is unchanged.
    /// For `Log::for_sink` (the desktop app) it is what finally lets an
    /// embedder see a prompt's body at all.
    pub fn writer(&self) -> LogWriter<'_> {
        LogWriter { log: self }
    }
```

And after the `impl Log` block:

```rust
/// See [`Log::writer`]. Locks the log's sink per `write` call and releases
/// it immediately — never across the guard's lifetime. Callers hold one of
/// these for a whole prompt phase while also calling `event`/`row`, and a
/// held lock would deadlock the first such call.
pub struct LogWriter<'a> {
    log: &'a Log,
}

impl std::io::Write for LogWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut state = self.log.state.lock().unwrap();
        if state.status_active {
            // A prompt must not be drawn on top of the watch countdown.
            state.out.write_all(b"\r\x1b[K")?;
            state.status_active = false;
        }
        state.out.write_all(buf)?;
        state.out.flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.log.state.lock().unwrap().out.flush()
    }
}
```

- [ ] **Step 2: Add the two `LogWriter` unit tests**

In `src/log.rs`'s `mod tests`:

```rust
    #[test]
    fn writer_passes_bytes_through_verbatim() {
        use std::io::Write;
        let buf = Buf::default();
        let log = Log::for_test(ColorMode::Plain, Box::new(buf.clone()));
        let mut w = log.writer();
        write!(w, "[k] keep local  [r] use dev > ").unwrap();
        assert_eq!(buf.text(), "[k] keep local  [r] use dev > ");
    }

    #[test]
    fn writer_does_not_deadlock_against_event() {
        use std::io::Write;
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 60 + 14),
        );
        // The shape `resolve_conflicts` uses: a live writer across events.
        let mut w = log.writer();
        write!(w, "a").unwrap();
        log.event(Action::Sync, "in between");
        write!(w, "b").unwrap();
        assert_eq!(buf.text(), "a12:01:14 sync   in between\nb");
    }
```

- [ ] **Step 3: Swap the sync executor's sink**

In `src/cli/sync/execute.rs`, replace

```rust
    let stderr = std::io::stderr();
    let mut stderr_lock = stderr.lock();
```

with

```rust
    // Prompt output goes through the renderer, not raw stderr: under
    // `Log::new` the sink IS stderr (identical bytes), and under
    // `Log::for_sink` it is the only way an embedder sees the diff.
    let mut prompt_out = progress.writer();
```

Rename every `stderr_lock` in this file to `prompt_out` — the local, the argument at the `:679` call, and the three `&mut *stderr_lock` uses. Change the helper's parameter type from

```rust
    stderr_lock: &mut std::io::StderrLock<'_>,
```

to

```rust
    prompt_out: &mut dyn std::io::Write,
```

`&mut dyn Write` implements `Write`, so the `W: Write` generics at the call sites still resolve.

- [ ] **Step 4: Swap the pull-side drift sink**

In `src/cli/pull/common.rs`, replace

```rust
    let stderr = std::io::stderr();
    let resolution = prompt_resolve(
        crate::cli::stdin_coord::CoordinatorStdin::new(),
        stderr.lock(),
```

with

```rust
    let resolution = prompt_resolve(
        crate::cli::stdin_coord::CoordinatorStdin::new(),
        progress.writer(),
```

`progress: &Arc<Log>` is already a parameter of this function.

- [ ] **Step 5: Build and run the whole suite**

Run:
```bash
cargo test 2>&1 | tail -30
```
Expected: everything green, **including `conflict_prompt_bytes_are_pinned` and `remote_delete_prompt_bytes_are_pinned` from Task 1, unmodified**. If either pin fails, the bytes moved — fix the code, never the pin.

- [ ] **Step 6: Confirm no `stderr` sink is left in a prompt path**

Run:
```bash
grep -rn "stderr().lock()\|stderr.lock()" src/cli/
```
Expected: no hits in `sync/execute.rs` or `pull/common.rs`. Hits elsewhere (the four `eprint!` sites) are Task 3's.

- [ ] **Step 7: Commit**

```bash
git add src/log.rs src/cli/sync/execute.rs src/cli/pull/common.rs
git commit -m "feat(log): add Log::writer and route prompt bodies through it

The conflict, remote-delete and push-drift prompts wrote their diff to
stderr().lock(), so an embedder consuming Log::for_sink never saw a byte
of it — a dialog rendered from that stream would have had no diff in it.

LogWriter locks per write, not for the guard's lifetime: resolve_conflicts
holds the sink across many progress.event() calls, and a held lock would
deadlock the first one.

Terminal bytes are unchanged; the two prompt pins prove it.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Route the four raw question lines through the log writer

**Files:**
- Modify: `src/cli/push/deletes.rs`
- Modify: `src/cli/push/mdh.rs`
- Modify: `src/cli/push/mdh_data.rs`

**Interfaces:**
- Consumes: `Log::writer` from Task 2.
- Produces: `resolve_delete_drift(progress: &Arc<Log>, interactive: bool, kind: &str, slug: &str)` — gains a leading `progress` parameter. The other three sites keep their signatures.

- [ ] **Step 1: The object-delete gate**

In `src/cli/push/deletes.rs`, replace

```rust
    eprint!("Proceed with deletion? [y/N] ");
    std::io::stderr().flush().ok();
```

with

```rust
    let mut q = progress.writer();
    write!(q, "Proceed with deletion? [y/N] ").ok();
    q.flush().ok();
    drop(q);
```

Keep the comment above it, but correct it — the question no longer "stays on raw stderr":

```rust
    // The question is written through the renderer rather than emitted as a
    // Log event: it must sit on the cursor's line for the answer to be typed
    // after it, which a timestamped event line cannot do. Under `Log::new`
    // the renderer's sink is stderr, so the terminal sees the same bytes.
```

- [ ] **Step 2: The delete-drift resolver**

Still in `deletes.rs`, change the signature and body:

```rust
fn resolve_delete_drift(
    progress: &Arc<Log>,
    interactive: bool,
    kind: &str,
    slug: &str,
) -> Result<DeleteDriftChoice> {
    if !interactive {
        // Non-TTY (CI / --yes): fall back to skip with warning so a
        // drifted delete never silently destroys someone else's work.
        progress.event(
            Action::Warn,
            &format!(
                "{kind}/{slug}: local file deleted but remote modified since last sync; \
                 skipping (run `rdc sync <env>` to retry)."
            ),
        );
        return Ok(DeleteDriftChoice::Skip);
    }
    let mut q = progress.writer();
    writeln!(q).ok();
    writeln!(
        q,
        "{kind}/{slug}: local file deleted, but remote has been modified since the last pull."
    )
    .ok();
    write!(q, "[k]eep delete  [r]estore  [s]kip  [a]bort > ").ok();
    q.flush().ok();
    drop(q);
    let ans = crate::cli::stdin_coord::read_line_coordinated()?
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match ans.as_str() {
        "k" | "keep" => Ok(DeleteDriftChoice::KeepDelete),
        "r" | "restore" => Ok(DeleteDriftChoice::Restore),
        "s" | "skip" | "" => Ok(DeleteDriftChoice::Skip),
        "a" | "abort" => Ok(DeleteDriftChoice::Abort),
        other => {
            progress.event(Action::Warn, &format!("unrecognised choice '{other}'; skipping"));
            Ok(DeleteDriftChoice::Skip)
        }
    }
}
```

Note the two `eprintln!`s in the non-interactive and unrecognised branches become `progress.event(Action::Warn, …)`. Those are warnings, not questions — an event line is the right shape and it reaches an embedder.

Update the single call site in `delete_one`:

```rust
        match resolve_delete_drift(progress, interactive, kind, slug)? {
```

- [ ] **Step 3: The two MDH gates**

In `src/cli/push/mdh.rs`, inside `prompt_confirm_index_drops`:

```rust
    progress.with_prompt(|| -> Result<bool> {
        use std::io::Write;
        let mut q = progress.writer();
        write!(q, "Proceed with the drop(s)? [y/N] ").ok();
        q.flush().ok();
        drop(q);
        let ans = crate::cli::stdin_coord::read_line_coordinated()?
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        Ok(ans == "y" || ans == "yes")
    })
```

In `src/cli/push/mdh_data.rs`, inside `prompt_confirm_row_deletes`, make the identical change with the text `"Proceed with the deletion(s)? [y/N] "`.

- [ ] **Step 4: Add a capture test for all four question strings**

In `src/cli/push/deletes.rs`'s `mod tests` (create the module if the file has none):

```rust
    /// The four destructive questions must reach a `Log` sink, not stderr —
    /// an embedder that cannot see the question cannot render a dialog for
    /// it. Pins the exact wording too: these are the strings a user reads
    /// before authorising a delete.
    #[test]
    fn delete_gate_question_reaches_the_log_sink() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let buf = Buf::default();
        let log = crate::log::Log::for_sink(
            crate::cli::resolve::ColorMode::Plain,
            Box::new(buf.clone()),
        );
        let mut w = log.writer();
        write!(w, "Proceed with deletion? [y/N] ").unwrap();
        drop(w);
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text, "Proceed with deletion? [y/N] ");
    }
```

`Log::for_sink` is `pub(crate)`, so this test must live inside the crate — it does.

- [ ] **Step 5: Build and run**

Run:
```bash
cargo test 2>&1 | tail -30
```
Expected: all green, Task 1's pins included.

- [ ] **Step 6: Confirm the raw prints are gone**

Run:
```bash
grep -rn "eprint!(" src/cli/
```
Expected: no hits.

- [ ] **Step 7: Commit**

```bash
git add src/cli/push/deletes.rs src/cli/push/mdh.rs src/cli/push/mdh_data.rs
git commit -m "fix(push): route the four destructive questions through the renderer

7e96b89 moved the gates' object lists onto the Log but left the questions
themselves on raw stderr, so an embedder saw what would be deleted and
never saw what it was being asked. resolve_delete_drift gains a progress
parameter for the same reason, and its two warnings become event lines.

Under Log::new the sink is stderr, so the terminal is unchanged.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: The prompt route (core, inert until something installs one)

**Files:**
- Modify: `src/cli/stdin_coord.rs`

**Interfaces:**
- Produces, all `pub` from `crate::cli::stdin_coord`:
  - `struct PromptKey { pub key: char, pub label: String }`
  - `enum PromptKind { Conflict, RemoteDelete, PushDrift, BulkConfirm, DeleteGate, DeleteDrift, MdhIndexDrop, MdhRowDelete }`
  - `struct Prompt { pub kind: PromptKind, pub question: String, pub keys: Vec<PromptKey> }`
  - `trait PromptRoute: Send + Sync { fn ask(&self, prompt: &Prompt) -> Option<String>; }`
  - `fn install_route(route: Arc<dyn PromptRoute>) -> RouteGuard`
  - `fn announce(p: Prompt)`
  - `struct RouteGuard` (clears the thread's route on drop)

**Design note — why `announce` and not `ask(w, &prompt)`:** the spec's §6.3 sketched a single `ask` that writes the question and reads the answer. The three big resolvers take a generic `R: BufRead` input precisely so their unit tests can drive them with a `Cursor`, and folding the read into `ask` would route those tests through the coordinator and break them. `announce` splits the two halves instead: the site declares what it is asking, then writes and reads exactly as it does today. Tests that supply their own `Cursor` never reach `read_line_coordinated` and are untouched.

- [ ] **Step 1: Add the types and the thread-local**

At the top of `src/cli/stdin_coord.rs`, extend the imports:

```rust
use std::cell::RefCell;
use std::io::{self, BufRead, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
```

Then add:

```rust
/// One answerable choice in a prompt: the character the resolver matches on
/// and the words the terminal shows beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptKey {
    pub key: char,
    pub label: String,
}

impl PromptKey {
    pub fn new(key: char, label: &str) -> Self {
        Self { key, label: label.to_string() }
    }
}

/// Which decision is being asked. A non-terminal consumer uses this to
/// title its dialog; the resolvers do not branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Conflict,
    RemoteDelete,
    PushDrift,
    BulkConfirm,
    DeleteGate,
    DeleteDrift,
    MdhIndexDrop,
    MdhRowDelete,
}

/// What a blocked prompt is asking, in machine-readable form. `question` is
/// the same string the terminal shows, trailing `"> "` and all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub question: String,
    pub keys: Vec<PromptKey>,
}

impl Prompt {
    /// Fallback for a coordinated read whose site has not announced —
    /// a free-text answer with no offered keys. Keeps an un-announced read
    /// working rather than panicking.
    fn unknown() -> Self {
        Self { kind: PromptKind::Conflict, question: String::new(), keys: Vec::new() }
    }
}

/// Where a blocked prompt's question goes and where its answer comes from,
/// when the consumer is not a terminal. Installed per thread, because a
/// cycle never leaves its thread (engine concurrency is `buffer_unordered`
/// on the current task, never `tokio::spawn` — see `api::mod`) and the
/// process-global [`COORD`] has a single waiting slot, so two concurrent
/// watches sharing it would hang the first one.
pub trait PromptRoute: Send + Sync {
    /// Block until the consumer answers. `None` means end of input; every
    /// resolver already degrades safely on that (conflicts skip, gates
    /// read as `N`).
    fn ask(&self, prompt: &Prompt) -> Option<String>;
}

thread_local! {
    static ROUTE: RefCell<Option<Arc<dyn PromptRoute>>> = const { RefCell::new(None) };
    static PENDING: RefCell<Option<Prompt>> = const { RefCell::new(None) };
}

/// Install `route` for the calling thread until the returned guard drops.
#[must_use = "the route is uninstalled when the guard drops"]
pub fn install_route(route: Arc<dyn PromptRoute>) -> RouteGuard {
    ROUTE.with(|r| *r.borrow_mut() = Some(route));
    RouteGuard(())
}

pub struct RouteGuard(());

impl Drop for RouteGuard {
    fn drop(&mut self) {
        ROUTE.with(|r| *r.borrow_mut() = None);
        PENDING.with(|p| *p.borrow_mut() = None);
    }
}

/// Declare what the next coordinated read is asking. Call immediately
/// before writing the question. A no-op when no route is installed, which
/// is every CLI invocation.
pub fn announce(p: Prompt) {
    if ROUTE.with(|r| r.borrow().is_some()) {
        PENDING.with(|slot| *slot.borrow_mut() = Some(p));
    }
}
```

- [ ] **Step 2: Give the route first refusal in `read_line_coordinated`**

Replace the body of `read_line_coordinated`, keeping its doc comment and extending it:

```rust
pub fn read_line_coordinated() -> io::Result<Option<String>> {
    maybe_ring_bell();
    // A thread-local route (the desktop app) outranks the process-global
    // coordinator (`rdc sync --watch` on a TTY), which outranks real stdin.
    // The CLI never installs a route, so its path is unchanged.
    if let Some(route) = ROUTE.with(|r| r.borrow().clone()) {
        let prompt = PENDING
            .with(|p| p.borrow_mut().take())
            .unwrap_or_else(Prompt::unknown);
        return Ok(route.ask(&prompt));
    }
    if let Some(coord) = COORD.get() {
        return Ok(coord.recv_line());
    }
    let mut s = String::new();
    if io::stdin().read_line(&mut s)? == 0 {
        return Ok(None);
    }
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    Ok(Some(s))
}
```

- [ ] **Step 3: Add the routing tests**

In `stdin_coord.rs`'s `mod tests`:

```rust
    struct Canned {
        answer: String,
        seen: Mutex<Vec<Prompt>>,
    }
    impl PromptRoute for Canned {
        fn ask(&self, prompt: &Prompt) -> Option<String> {
            self.seen.lock().unwrap().push(prompt.clone());
            Some(self.answer.clone())
        }
    }

    #[test]
    fn an_installed_route_answers_and_sees_the_announced_prompt() {
        let route = Arc::new(Canned { answer: "k".into(), seen: Mutex::new(Vec::new()) });
        let guard = install_route(route.clone());
        announce(Prompt {
            kind: PromptKind::DeleteGate,
            question: "Proceed with deletion? [y/N] ".into(),
            keys: vec![PromptKey::new('y', "yes"), PromptKey::new('n', "no")],
        });
        assert_eq!(read_line_coordinated().unwrap(), Some("k".to_string()));
        let seen = route.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, PromptKind::DeleteGate);
        assert_eq!(seen[0].keys.len(), 2);
        drop(guard);
    }

    #[test]
    fn a_pending_prompt_is_consumed_once() {
        let route = Arc::new(Canned { answer: "s".into(), seen: Mutex::new(Vec::new()) });
        let guard = install_route(route.clone());
        announce(Prompt {
            kind: PromptKind::Conflict,
            question: "q".into(),
            keys: vec![PromptKey::new('s', "skip")],
        });
        let _ = read_line_coordinated().unwrap();
        let _ = read_line_coordinated().unwrap();
        let seen = route.seen.lock().unwrap();
        // Second read saw the fallback, not a stale copy of the first.
        assert_eq!(seen[0].question, "q");
        assert_eq!(seen[1].question, "");
        drop(guard);
    }

    /// The constraint that ruled out the process-global coordinator: two
    /// watches must be able to prompt at the same time without either
    /// answer landing on the wrong thread.
    #[test]
    fn routes_are_per_thread_and_do_not_cross_talk() {
        let a = Arc::new(Canned { answer: "A".into(), seen: Mutex::new(Vec::new()) });
        let b = Arc::new(Canned { answer: "B".into(), seen: Mutex::new(Vec::new()) });

        let (a2, b2) = (a.clone(), b.clone());
        let ta = std::thread::spawn(move || {
            let _g = install_route(a2);
            announce(Prompt { kind: PromptKind::Conflict, question: "from-a".into(), keys: vec![] });
            read_line_coordinated().unwrap()
        });
        let tb = std::thread::spawn(move || {
            let _g = install_route(b2);
            announce(Prompt { kind: PromptKind::Conflict, question: "from-b".into(), keys: vec![] });
            read_line_coordinated().unwrap()
        });

        assert_eq!(ta.join().unwrap(), Some("A".to_string()));
        assert_eq!(tb.join().unwrap(), Some("B".to_string()));
        assert_eq!(a.seen.lock().unwrap()[0].question, "from-a");
        assert_eq!(b.seen.lock().unwrap()[0].question, "from-b");
    }

    #[test]
    fn no_route_leaves_the_global_coordinator_path_intact() {
        // Nothing installed on this thread: `read_line_coordinated` must not
        // touch the thread-local branch. Exercised indirectly by the existing
        // `delivers_to_waiting_prompt` test, which still passes.
        assert!(ROUTE.with(|r| r.borrow().is_none()));
    }
```

- [ ] **Step 4: Build and run**

Run:
```bash
cargo test --lib cli::stdin_coord 2>&1 | tail -20
```
Expected: the four new tests pass and the three pre-existing ones (`try_deliver_returns_line_when_no_prompt_waiting`, `delivers_to_waiting_prompt`, `bell_arm_take_debounce_rearm_and_tty_gate`, `coordinator_stdin_buffers_one_delivered_line_with_newline`) pass unmodified.

Run:
```bash
cargo test 2>&1 | tail -10
```
Expected: whole suite green — nothing installs a route yet, so nothing else can have changed.

- [ ] **Step 5: Commit**

```bash
git add src/cli/stdin_coord.rs
git commit -m "feat(cli): thread-local prompt routing for non-terminal consumers

read_line_coordinated now offers a blocked prompt to a thread-local route
before falling back to the global coordinator and then real stdin. The CLI
installs no route, so its path is unchanged.

Per-thread rather than global because the global coordinator has one
waiting slot: two concurrent watches sharing it would leave the first
hanging forever. A cycle never leaves its thread, so a thread-local scopes
exactly one route per watch.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Announce at every prompt site

**Files:**
- Modify: `src/cli/resolve.rs`
- Modify: `src/cli/push/deletes.rs`
- Modify: `src/cli/push/mdh.rs`
- Modify: `src/cli/push/mdh_data.rs`

**Interfaces:**
- Consumes: `Prompt`, `PromptKey`, `PromptKind`, `announce` from Task 4.
- Produces: every coordinated read is preceded by an `announce`. No prompt text changes.

**Rule:** `announce` goes immediately before the existing `write!` of the question, inside any re-prompt loop, so a re-ask re-announces. The `keys` you build must mirror the question string exactly — same characters, same order.

- [ ] **Step 1: The conflict prompt**

In `src/cli/resolve.rs`, in `prompt_resolve_with_bytes_and_color`'s loop, immediately before `write!(output, "{}", colorize_prompt(&prompt_text, mode))?;`:

```rust
        let mut keys = vec![
            PromptKey::new('k', "keep local"),
            PromptKey::new('r', &format!("use {env}")),
            PromptKey::new('e', "edit"),
        ];
        if hunk_count >= 2 {
            keys.push(PromptKey::new('h', "hunk-by-hunk"));
        }
        keys.push(PromptKey::new('s', "skip (shadow file)"));
        keys.push(PromptKey::new('a', "abort"));
        if bulk.is_some() {
            keys.push(PromptKey::new('K', "keep ALL local"));
            keys.push(PromptKey::new('R', &format!("use {env} for ALL")));
        }
        crate::cli::stdin_coord::announce(crate::cli::stdin_coord::Prompt {
            kind: crate::cli::stdin_coord::PromptKind::Conflict,
            question: prompt_text.clone(),
            keys,
        });
```

Add `use crate::cli::stdin_coord::{Prompt, PromptKey, PromptKind};` at the top of the file if you prefer unqualified names; either is fine, be consistent within the file.

- [ ] **Step 2: The bulk confirmation**

In `confirm_bulk`, before `write!(output, "{}", colorize_prompt("Continue? [y/N] > ", mode))?;`:

```rust
    crate::cli::stdin_coord::announce(crate::cli::stdin_coord::Prompt {
        kind: crate::cli::stdin_coord::PromptKind::BulkConfirm,
        question: "Continue? [y/N] > ".into(),
        keys: vec![PromptKey::new('y', "yes"), PromptKey::new('n', "no")],
    });
```

This one matters: `confirm_bulk` is a nested read inside the conflict prompt. Without its own announce it would inherit the conflict's already-consumed slot and the app would show a stale question.

- [ ] **Step 3: The remote-delete prompt**

In `prompt_remote_delete_with_color`, before its question `write!`, with keys mirroring its `[k]/[r]/[s]/[a]` label text (read the literal in the function and copy the wording verbatim — it names the env like the conflict prompt does):

```rust
        crate::cli::stdin_coord::announce(crate::cli::stdin_coord::Prompt {
            kind: crate::cli::stdin_coord::PromptKind::RemoteDelete,
            question: prompt_text.clone(),
            keys: vec![
                PromptKey::new('k', "restore on env"),
                PromptKey::new('r', "mirror the deletion locally"),
                PromptKey::new('s', "skip"),
                PromptKey::new('a', "abort"),
            ],
        });
```

- [ ] **Step 4: The push-drift prompt**

`src/cli/pull/common.rs` calls `prompt_resolve`, which funnels into `prompt_resolve_with_bytes_and_color` — Step 1 already covers it. Change nothing here, but set `kind` correctly is not possible from inside the shared function; leaving it as `Conflict` is correct, because it is the same decision with the same keys. Note that in a comment at the call site in `pull/common.rs`:

```rust
    // Announced as PromptKind::Conflict by the shared resolver — this is the
    // same decision with the same keys, reached mid-cycle rather than in the
    // classify phase.
```

- [ ] **Step 5: The object-delete gate**

In `src/cli/push/deletes.rs`, before the `write!(q, "Proceed with deletion? [y/N] ")` added in Task 3:

```rust
    crate::cli::stdin_coord::announce(crate::cli::stdin_coord::Prompt {
        kind: crate::cli::stdin_coord::PromptKind::DeleteGate,
        question: "Proceed with deletion? [y/N] ".into(),
        keys: vec![
            crate::cli::stdin_coord::PromptKey::new('y', "delete them"),
            crate::cli::stdin_coord::PromptKey::new('n', "cancel"),
        ],
    });
```

- [ ] **Step 6: The delete-drift resolver**

In `resolve_delete_drift`, before its `write!`:

```rust
    crate::cli::stdin_coord::announce(crate::cli::stdin_coord::Prompt {
        kind: crate::cli::stdin_coord::PromptKind::DeleteDrift,
        question: "[k]eep delete  [r]estore  [s]kip  [a]bort > ".into(),
        keys: vec![
            crate::cli::stdin_coord::PromptKey::new('k', "keep delete"),
            crate::cli::stdin_coord::PromptKey::new('r', "restore"),
            crate::cli::stdin_coord::PromptKey::new('s', "skip"),
            crate::cli::stdin_coord::PromptKey::new('a', "abort"),
        ],
    });
```

- [ ] **Step 7: The two MDH gates**

In `src/cli/push/mdh.rs::prompt_confirm_index_drops`, inside the `with_prompt` closure and before the `write!`:

```rust
        crate::cli::stdin_coord::announce(crate::cli::stdin_coord::Prompt {
            kind: crate::cli::stdin_coord::PromptKind::MdhIndexDrop,
            question: "Proceed with the drop(s)? [y/N] ".into(),
            keys: vec![
                crate::cli::stdin_coord::PromptKey::new('y', "drop them"),
                crate::cli::stdin_coord::PromptKey::new('n', "cancel"),
            ],
        });
```

In `src/cli/push/mdh_data.rs::prompt_confirm_row_deletes`, same position:

```rust
        crate::cli::stdin_coord::announce(crate::cli::stdin_coord::Prompt {
            kind: crate::cli::stdin_coord::PromptKind::MdhRowDelete,
            question: "Proceed with the deletion(s)? [y/N] ".into(),
            keys: vec![
                crate::cli::stdin_coord::PromptKey::new('y', "delete them"),
                crate::cli::stdin_coord::PromptKey::new('n', "cancel"),
            ],
        });
```

- [ ] **Step 8: Prove every coordinated read has an announce**

Run:
```bash
grep -rn "read_line_coordinated()" src/ | grep -v "^src/cli/stdin_coord.rs"
grep -rn "stdin_coord::announce" src/ | wc -l
```
Expected: 4 call sites of `read_line_coordinated` outside the coordinator (the two in `deletes.rs`, one in `mdh.rs`, one in `mdh_data.rs`), and **8** announces (4 for those, plus conflict, bulk, remote-delete — the `CoordinatorStdin`-driven ones — and none double-counted). If the counts do not line up, a site is unannounced and will show the app an empty question.

- [ ] **Step 9: Build and run**

Run:
```bash
cargo test 2>&1 | tail -20
```
Expected: whole suite green, Task 1's pins included — `announce` is a no-op without a route, so no byte moved.

- [ ] **Step 10: Commit**

```bash
git add src/cli/resolve.rs src/cli/pull/common.rs src/cli/push/deletes.rs src/cli/push/mdh.rs src/cli/push/mdh_data.rs
git commit -m "feat(cli): announce what each blocking prompt is asking

Eight sites now declare their kind, question and offered keys before they
write and read. Inert on the CLI (no route is installed) and the prompt
text is untouched, so the pins still hold.

confirm_bulk gets its own announce: it is a nested read inside the
conflict prompt, and without one it would inherit an already-consumed
slot and show a stale question.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: `secrets::force_relogin`

**Files:**
- Modify: `src/secrets.rs`

**Interfaces:**
- Produces: `pub async fn force_relogin(project_root: &Path, env: &str, api_base: &str) -> Result<String>` — ignores the cached token, logs in from the secrets file's persisted `username`/`password`, writes the new token back, returns it.

Why: `resolve_token` returns the *same* token while it is clock-valid (`secrets.rs:216`), so calling it again after a 401 changes nothing. And `cli::auth::refresh_token_for_401` is CWD-based and only reads `RDC_USER_<ENV>`/`RDC_PASS_<ENV>` env vars (`auth.rs:198-230`) — never the credentials the desktop persists in the secrets file.

- [ ] **Step 1: Write the failing test**

In `src/secrets.rs`'s `mod tests`:

```rust
    #[test]
    fn force_relogin_refuses_without_persisted_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        write_secrets_file(tmp.path(), "dev", "a-token", None).unwrap();
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(force_relogin(tmp.path(), "dev", "https://acme.test/api/v1"))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("token") && msg.contains("dev"),
            "error must name the env and say the token was rejected: {msg}"
        );
    }
```

The happy path needs a live login endpoint, so it belongs in the opt-in live harness, not here. This test pins the branch that matters for correctness of the message a desktop user sees.

- [ ] **Step 2: Run it and watch it fail**

Run:
```bash
cargo test --lib secrets::tests::force_relogin 2>&1 | tail -10
```
Expected: FAIL — `cannot find function force_relogin`.

- [ ] **Step 3: Implement**

Add to `src/secrets.rs`, next to `resolve_token`:

```rust
/// Re-authenticate an env whose cached token was rejected (401), ignoring
/// the cache entirely.
///
/// [`resolve_token`] cannot do this: it returns the cached token whenever
/// `expires_at` is absent or still in the future, which is exactly the
/// state a revoked-but-unexpired token is in. And
/// `cli::auth::refresh_token_for_401` reads only `RDC_USER_<ENV>` /
/// `RDC_PASS_<ENV>`, never the credentials the desktop app persists in
/// `secrets/<env>.secrets.json`.
///
/// Token-auth projects have no credentials to re-login with, so this fails
/// with a message that tells the user what to do about it.
pub async fn force_relogin(project_root: &Path, env: &str, api_base: &str) -> Result<String> {
    let file = read_secrets_file(project_root, env)?;
    let (Some(username), Some(password)) = (file.username.as_deref(), file.password.as_deref())
    else {
        return Err(anyhow!(
            "the API token for env '{env}' was rejected (401), and this env has no saved \
             username/password to sign in with again. Update its token and retry."
        ));
    };
    if username.is_empty() || password.is_empty() {
        return Err(anyhow!(
            "the API token for env '{env}' was rejected (401), and this env's saved \
             credentials are incomplete. Update them and retry."
        ));
    }
    let token = crate::api::login(api_base, username, password)
        .await
        .with_context(|| format!("re-signing in to env '{env}' after a 401"))?;
    let expires_at = now_unix_secs().saturating_add(LOGIN_TOKEN_LIFETIME_SECS);
    write_secrets_file(project_root, env, &token, Some(expires_at))?;
    Ok(token)
}
```

`write_secrets_file` already preserves `username`/`password` when it rewrites the token — its own doc comment says so — so a re-login does not destroy the credentials it just used.

- [ ] **Step 4: Run**

Run:
```bash
cargo test --lib secrets 2>&1 | tail -10
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/secrets.rs
git commit -m "feat(secrets): force_relogin for a revoked-but-unexpired token

resolve_token hands back the cached token whenever expires_at is absent or
still in the future, which is exactly the state a revoked token is in, and
refresh_token_for_401 only ever reads the RDC_USER_/RDC_PASS_ env vars. An
embedded long-running watch needs neither.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Parameterise the watch loop

**Files:**
- Modify: `src/cli/sync/watch.rs`
- Modify: `src/cli/mod.rs` (the `run_watch` call site)

**Interfaces:**
- Produces:
  - `pub struct CancelToken(Arc<tokio::sync::Notify>, Arc<AtomicBool>)` with `pub fn new() -> Self`, `pub fn cancel(&self)`, `pub fn is_cancelled(&self) -> bool`, `pub async fn cancelled(&self)`.
  - `pub struct WatchConfig<'a> { pub env: &'a str, pub cwd: Option<&'a Path>, pub token: Option<String>, pub interactive: bool, pub allow_deletes: bool, pub no_push: bool, pub no_pull: bool, pub poll: Option<Duration>, pub verbose: bool, pub no_bell: bool }`
  - `pub type TokenRefresher = Arc<dyn Fn(String) -> BoxFuture<'static, Result<Option<String>>> + Send + Sync>` — given the env name, returns the new token (or `None` to mean "the caller refreshed the on-disk secrets in place").
  - `pub type StdinHook = Box<dyn FnOnce(tokio::sync::mpsc::Sender<CycleTrigger>, Arc<AtomicBool>, CancelToken) + Send>`
  - `pub async fn run_watch_with(cfg: WatchConfig<'_>, renderer: Arc<Log>, cancel: CancelToken, refresher: TokenRefresher, stdin_hook: Option<StdinHook>) -> Result<()>`
  - `pub async fn run_watch(...)` — unchanged public signature, now a thin caller of the above.

- [ ] **Step 1: Add `CancelToken`**

At the top of `src/cli/sync/watch.rs`:

```rust
/// Cooperative shutdown for a watch loop. The CLI wires Ctrl-C to it; an
/// embedder wires its own stop button. Cloneable, and `cancel` is
/// idempotent so a double stop is harmless.
#[derive(Clone)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self { flag: Arc::new(AtomicBool::new(false)), notify: Arc::new(tokio::sync::Notify::new()) }
    }
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    /// Resolves once cancelled. Returns immediately if already cancelled,
    /// so a cancel that lands before the `select!` is not lost.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}
```

- [ ] **Step 2: Thread cwd/token/cancel through `event_loop`**

Change `event_loop`'s signature to take `cfg: &WatchConfig<'_>` in place of `env`/`interactive`/`allow_deletes`/`no_push`/`no_pull`/`verbose`/`no_bell`, and `cancel: CancelToken` in place of `mut shutdown: tokio::sync::oneshot::Receiver<()>`, and `refresher: &TokenRefresher`.

Inside, replace

```rust
    let cwd = std::env::current_dir()?;
    let paths = crate::paths::Paths::for_env(&cwd, env);
```

with

```rust
    let cwd = match cfg.cwd {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let paths = crate::paths::Paths::for_env(&cwd, cfg.env);
```

Replace the shutdown arm

```rust
            _ = &mut shutdown => break,
```

with

```rust
            _ = cancel.cancelled() => break,
```

And both `run_cycle(...)` calls change their last two arguments from `None, None` to `cfg.cwd, cfg.token.clone()`.

The 401 arm changes from the hardcoded `refresh_token_for_401` to the injected refresher, and the retry uses whatever token comes back:

```rust
                    Err(e) if crate::api::anyhow_has_status(&e, 401) => {
                        if let Some(r) = renderer.as_ref() {
                            r.event(crate::log::Action::Auth, "token expired — refreshing");
                        } else {
                            eprintln!("auth: token expired");
                        }
                        let fresh = refresher(cfg.env.to_string()).await?;
                        let token = fresh.or_else(|| cfg.token.clone());
                        crate::cli::sync::run_cycle(
                            cfg.env, cfg.interactive, false, cfg.allow_deletes,
                            cfg.no_push, cfg.no_pull, None, renderer.clone(),
                            cfg.cwd, token,
                        ).await?
                    }
```

Leave every other line of the loop alone — the debounce, the coalescing drain, the unwatch/rewatch around our own writes, the `ResetOnDrop` and `CycleGuard` guards, and the transient/parse-error arms are all unchanged.

- [ ] **Step 3: Split `run_watch` into a thin caller plus `run_watch_with`**

`run_watch_with` holds everything that is not terminal-specific: the initial reconcile under the env lock, the two `renderer.event` start lines, the ticker task, the file watcher, and the `event_loop` call. It must **not** activate the stdin coordinator, must **not** install a Ctrl-C handler, and must **not** call `std::process::exit`. It ends with:

```rust
    renderer.finish_status();
    renderer.event(crate::log::Action::Done, "stopped watch");
    renderer.event(crate::log::Action::Watch, "stopped");
    Ok(())
```

`run_watch` keeps its current signature and becomes:

```rust
pub async fn run_watch(
    env: &str,
    interactive: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    poll_interval: Option<Duration>,
    verbose: bool,
    no_bell: bool,
) -> Result<()> {
    let renderer = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
    let cancel = CancelToken::new();

    // Ctrl-C → cancel.
    let c = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        c.cancel();
    });

    // TTY stdin ownership: the Enter-trigger reader and the prompt
    // coordinator. Embedders install a PromptRoute instead and never take
    // stdin, so this stays here rather than in run_watch_with.
    let manual_tx = spawn_stdin_reader_if_tty(&cancel);

    let refresher: TokenRefresher = Arc::new(|env: String| {
        Box::pin(async move {
            crate::cli::auth::refresh_token_for_401(&env).await?;
            Ok(None) // secrets file rewritten in place; run_cycle re-reads it
        })
    });

    run_watch_with(
        WatchConfig {
            env,
            cwd: None,
            token: None,
            interactive,
            allow_deletes,
            no_push,
            no_pull,
            poll: poll_interval,
            verbose,
            no_bell,
        },
        renderer,
        cancel,
        refresher,
        manual_tx,
    )
    .await?;

    // Exit the process directly instead of returning Ok(()) and letting
    // `main` fall into the tokio runtime's Drop. The stdin reader task is
    // parked in an uncancellable blocking read that only returns on EOF, so
    // dropping the multi-threaded runtime would hang the process after
    // Ctrl-C until the user also pressed Ctrl-D. Error paths still
    // propagate through `?` and are handled by main's exit(1).
    std::process::exit(0)
}
```

Keep the original `std::process::exit(0)` comment verbatim — it documents a real bug that was fixed once.

The stdin reader needs to feed `CycleTrigger::Manual` into the same channel the ticker uses, so it becomes a hook `run_watch_with` invokes after creating the channel:

```rust
/// Installed by the CLI to take ownership of the terminal's stdin. Given
/// the event sender and the `sync_running` flag (a mid-cycle keypress must
/// be dropped, not queued) plus the cancel token, it spawns the reader.
/// Embedders pass `None`: they answer prompts through a `PromptRoute` and
/// never touch stdin.
pub type StdinHook = Box<
    dyn FnOnce(tokio::sync::mpsc::Sender<CycleTrigger>, Arc<AtomicBool>, CancelToken) + Send,
>;
```

`run_watch` builds one that contains today's `if std::io::stdin().is_terminal() { let coord = stdin_coord::activate(); … }` block verbatim — move it, do not rewrite it. The `try_deliver`-then-fall-through routing and the `sync_running` gate are both load-bearing and already correct.

- [ ] **Step 4: Update the CLI dispatch**

`src/cli/mod.rs` calls `crate::cli::sync::watch::run_watch(...)` with eight positional arguments. Its signature is unchanged, so this file should need no edit — confirm with a build, and if the compiler disagrees, match the new arity rather than changing `run_watch`'s signature.

- [ ] **Step 5: Add the injection tests**

In `watch.rs`'s `mod tests`, alongside `event_loop_exits_cleanly_on_shutdown` (which must keep passing after being adapted to the new signature — adapt it, do not delete it):

```rust
    #[tokio::test]
    async fn cancel_token_resolves_even_when_cancelled_first() {
        let c = CancelToken::new();
        c.cancel();
        // Would hang forever if `cancelled()` only awaited the Notify.
        tokio::time::timeout(std::time::Duration::from_secs(1), c.cancelled())
            .await
            .expect("a pre-cancelled token must resolve immediately");
    }

    #[tokio::test]
    async fn event_loop_uses_the_injected_cwd_not_the_process_cwd() {
        // A project at `tmp`, and a process CWD somewhere else entirely.
        // With cfg.cwd wired through, the loop must lock tmp's env lock and
        // never look at the process CWD.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".rdc/state")).unwrap();
        let (_tx, rx) = tokio::sync::mpsc::channel::<CycleTrigger>(8);
        let cancel = CancelToken::new();
        cancel.cancel(); // shut down before any cycle runs

        let cfg = WatchConfig {
            env: "test",
            cwd: Some(tmp.path()),
            token: Some("tok".into()),
            interactive: false,
            allow_deletes: false,
            no_push: false,
            no_pull: false,
            poll: None,
            verbose: false,
            no_bell: true,
        };
        let refresher: TokenRefresher = Arc::new(|_| Box::pin(async { Ok(None) }));
        let result = event_loop(
            &cfg,
            rx,
            cancel,
            None,
            tmp.path().join("envs/test"),
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(tokio::sync::Notify::new()),
            &refresher,
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
    }
```

Note the second test does **not** `std::env::set_current_dir` — that is the point. The existing `event_loop_exits_cleanly_on_shutdown` does change the process CWD; adapt it to pass `cwd: None` so it keeps covering the CLI path.

- [ ] **Step 6: Build and run the watch tests**

Run:
```bash
cargo test --lib cli::sync::watch 2>&1 | tail -20
cargo test --test cli_sync sync_watch 2>&1 | tail -20
```
Expected: green, including the three pre-existing integration tests
`sync_watch_initial_reconcile_pulls_remote_creates`,
`sync_watch_poll_catches_remote_drift`, and the no-meta-confirmation one.
Those call `run_watch` directly and must not need editing — if they do, the public signature moved and it should not have.

- [ ] **Step 7: Full suite**

Run:
```bash
cargo test 2>&1 | tail -10
```
Expected: green.

- [ ] **Step 8: Commit**

```bash
git add src/cli/sync/watch.rs src/cli/mod.rs
git commit -m "refactor(watch): parameterise the loop so a non-CLI caller can drive it

event_loop read std::env::current_dir() and passed None/None for
run_cycle's cwd and token overrides, which run_cycle has accepted all
along. It now takes a WatchConfig, a CancelToken in place of the Ctrl-C
oneshot, and an injected token refresher.

run_watch keeps its signature and its process::exit(0) — the stdin reader
still parks in an uncancellable read — and remains the only caller that
touches stdin or signals.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: The embedding entry points

**Files:**
- Modify: `src/cli/sync/embed.rs`

**Interfaces:**
- Produces:
  - `pub async fn sync_logged(cwd: &Path, env: &str, token: &str, opts: EmbedSyncOptions, log_sink: Box<dyn Write + Send>) -> Result<CycleOutcome>`
  - `pub struct EmbedSyncOptions { pub interactive: bool, pub allow_deletes: bool, pub no_push: bool, pub no_pull: bool, pub dry_run: bool, pub conflict: Option<ConflictStrategy> }` with a `Default` that is the app's policy: `interactive: true, allow_deletes: false, no_push: false, no_pull: false, dry_run: false, conflict: None`.
  - `pub async fn watch_logged(cwd: &Path, env: &str, api_base: &str, token: String, poll: Option<Duration>, log_sink: Box<dyn Write + Send>, route: Arc<dyn PromptRoute>, cancel: CancelToken) -> Result<()>`
- Removes: `sync_no_push_logged`, `sync_push_logged`. Keeps `sync_no_push` (`tests/embed_sync.rs`).

- [ ] **Step 1: Replace the two logged wrappers**

```rust
/// How an embedder wants one reconciliation cycle run.
///
/// `Default` is the desktop app's policy, and the three interlocking parts
/// of it are load-bearing: `interactive: true` so a gate prompts instead of
/// bailing, `allow_deletes: false` so the gate is actually reached, and
/// `conflict: None` so a divergence is asked about rather than decided.
/// Changing any one of them silently removes a user's say over a delete.
pub struct EmbedSyncOptions {
    pub interactive: bool,
    pub allow_deletes: bool,
    pub no_push: bool,
    pub no_pull: bool,
    pub dry_run: bool,
    pub conflict: Option<ConflictStrategy>,
}

impl Default for EmbedSyncOptions {
    fn default() -> Self {
        Self {
            interactive: true,
            allow_deletes: false,
            no_push: false,
            no_pull: false,
            dry_run: false,
            conflict: None,
        }
    }
}

/// Run one reconciliation cycle, streaming rdc's rendered log into
/// `log_sink` — one `write` per line.
///
/// Lines carry rdc's normal ANSI colour so the embedder can preserve it,
/// and the log is non-TTY (no cursor redraws). Because a renderer is
/// supplied, `run_cycle` omits its cycle-closing summary; the caller adds
/// its own completion line.
pub async fn sync_logged(
    cwd: &Path,
    env: &str,
    token: &str,
    opts: EmbedSyncOptions,
    log_sink: Box<dyn std::io::Write + Send>,
) -> Result<CycleOutcome> {
    let paths = crate::paths::Paths::for_env(cwd, env);
    let _lock = crate::cli::sync::lock::EnvLock::acquire(
        &paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;
    let renderer = Log::for_sink(crate::cli::resolve::ColorMode::Color, log_sink);
    crate::cli::sync::run_cycle(
        env,
        opts.interactive,
        opts.dry_run,
        opts.allow_deletes,
        opts.no_push,
        opts.no_pull,
        opts.conflict,
        Some(renderer),
        Some(cwd),
        Some(token.to_string()),
    )
    .await
}
```

Delete `sync_no_push_logged` and `sync_push_logged`.

- [ ] **Step 2: Add `watch_logged`**

```rust
/// Run a watch loop against `cwd`/`env` until `cancel` fires, streaming the
/// log into `log_sink` and routing every blocking prompt to `route`.
///
/// Unlike `cli::sync::watch::run_watch` this owns no stdin, installs no
/// signal handler, and returns normally instead of exiting the process.
/// 401s are resolved with `secrets::force_relogin` rather than the CLI's
/// interactive refresh, because the app's credentials live in the secrets
/// file, not in `RDC_USER_<ENV>` / `RDC_PASS_<ENV>`.
#[allow(clippy::too_many_arguments)]
pub async fn watch_logged(
    cwd: &Path,
    env: &str,
    api_base: &str,
    token: String,
    poll: Option<std::time::Duration>,
    log_sink: Box<dyn std::io::Write + Send>,
    route: std::sync::Arc<dyn crate::cli::stdin_coord::PromptRoute>,
    cancel: crate::cli::sync::watch::CancelToken,
) -> Result<()> {
    // Installed for this thread only, for the whole watch. A cycle never
    // leaves its thread, so this scopes exactly one route per watch.
    let _route_guard = crate::cli::stdin_coord::install_route(route);

    let renderer = Log::for_sink(crate::cli::resolve::ColorMode::Color, log_sink);

    let root = cwd.to_path_buf();
    let base = api_base.to_string();
    let refresher: crate::cli::sync::watch::TokenRefresher =
        std::sync::Arc::new(move |env: String| {
            let root = root.clone();
            let base = base.clone();
            Box::pin(async move {
                let t = crate::secrets::force_relogin(&root, &env, &base).await?;
                Ok(Some(t))
            })
        });

    crate::cli::sync::watch::run_watch_with(
        crate::cli::sync::watch::WatchConfig {
            env,
            cwd: Some(cwd),
            token: Some(token),
            interactive: true,
            allow_deletes: false,
            no_push: false,
            no_pull: false,
            poll,
            verbose: false,
            no_bell: true, // a terminal BEL means nothing in a GUI process
        },
        renderer,
        cancel,
        refresher,
        None, // no stdin reader: prompts arrive through `route`
    )
    .await
}
```

- [ ] **Step 3: Add a two-way embed test**

`tests/embed_sync.rs` already stands up a `wiremock::MockServer` with the organization GET and all thirteen empty listings, then seeds an `rdc.toml` pointing at it. Create `tests/embed_sync_two_way.rs` by copying that whole fixture verbatim, then:

- return one queue from `GET /queues` instead of an empty list, and write the matching `envs/test/queues/<slug>.json` plus a lockfile entry so the object is *known* rather than new;
- mount a `Mock::given(method("PATCH")).and(path("/queues/1"))` with `.expect(1)`;
- edit the local file so it diverges from the mocked remote body;
- run `sync_logged(cwd, "test", "tok", EmbedSyncOptions::default(), Box::new(std::io::sink()))`.

`MockServer` verifies its `.expect(1)` on drop, so the PATCH assertion needs no explicit check. Head the test with:

```rust
//! A locally-edited object is PUSHED by `sync_logged`'s default options,
//! where `sync_no_push` (tests/embed_sync.rs) leaves it alone. This is the
//! behaviour change the desktop app is adopting; pinned here so it cannot
//! regress into silence.
```

Add a second test in the same file asserting the policy itself, since it is the part a future refactor is most likely to "simplify":

```rust
#[test]
fn default_embed_options_prompt_rather_than_decide() {
    let o = rdc::cli::sync::embed::EmbedSyncOptions::default();
    assert!(o.interactive, "false would bail! on a pending delete and kill a watch");
    assert!(!o.allow_deletes, "true would skip the delete gate entirely");
    assert!(o.conflict.is_none(), "Some(_) would resolve divergence without asking");
    assert!(!o.no_push && !o.no_pull, "the app's sync is two-way");
}
```

- [ ] **Step 4: Build and run**

Run:
```bash
cargo test 2>&1 | tail -20
```
Expected: green. `desktop/rust` is a separate workspace and is **expected to be broken** at this point — it still calls `sync_no_push_logged`. Task 9 fixes it. Do not build it here.

- [ ] **Step 5: Commit**

```bash
git add src/cli/sync/embed.rs tests/embed_sync_two_way.rs
git commit -m "feat(embed): one two-way sync_logged, plus watch_logged

Replaces sync_no_push_logged and sync_push_logged, whose split existed for
the desktop's promote push and its pull-only sync — neither of which the
app does any more. EmbedSyncOptions::default() is the app's policy, and
its three interlocking parts are documented where they are defined.

watch_logged owns no stdin, installs no signal handler, returns normally,
and refreshes a 401 from the secrets file rather than RDC_USER_/RDC_PASS_.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: Bridge — two-way sync and the new stream phases

**Files:**
- Modify: `desktop/rust/src/api/rdc.rs`
- Regenerate: `desktop/lib/src/rust/**`, `desktop/rust/src/frb_generated.rs`
- Modify: `desktop/lib/src/app_state.dart` (switch exhaustiveness only)

**Interfaces:**
- Produces: `SyncPhase` with `Started`, `Log { line }`, `Prompt { id, kind, question, keys }`, `PromptResolved { id }`, `Idle { next_poll_secs }`, `Done { file_count }`, `Error { message }`, `Stopped`. Plus `struct PromptChoice { key: String, label: String }` and `enum PromptKindDto` mirroring `stdin_coord::PromptKind`.
- `sync_env` keeps its Dart signature and becomes two-way.

- [ ] **Step 1: Extend `SyncPhase` and add the DTOs**

In `desktop/rust/src/api/rdc.rs`:

```rust
/// One answerable choice, as offered to the UI. `key` is a String rather
/// than a char because FRB has no char; it is always exactly one character.
#[derive(Debug, Clone)]
pub struct PromptChoice {
    pub key: String,
    pub label: String,
}

/// Mirrors `rdc::cli::stdin_coord::PromptKind` across the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKindDto {
    Conflict,
    RemoteDelete,
    PushDrift,
    BulkConfirm,
    DeleteGate,
    DeleteDrift,
    MdhIndexDrop,
    MdhRowDelete,
}

#[derive(Debug, Clone)]
pub enum SyncPhase {
    Started,
    /// One line of rdc's real, rendered sync log (plain text, no color).
    Log { line: String },
    /// A cycle is blocked waiting for an answer. Reply with `answer_prompt`
    /// using this `id`. The diff/list this refers to has already arrived as
    /// `Log` lines.
    Prompt {
        id: u64,
        kind: PromptKindDto,
        question: String,
        keys: Vec<PromptChoice>,
    },
    /// The prompt with this id no longer needs an answer (the watch stopped,
    /// or the cycle was torn down). Close the dialog.
    PromptResolved { id: u64 },
    /// A watch is between cycles. `next_poll_secs` is None when polling is
    /// disabled. rdc's own countdown never reaches an embedder — its
    /// in-place status line is a no-op off a TTY — so the app draws its own.
    Idle { next_poll_secs: Option<u64> },
    Done { file_count: u64 },
    Error { message: String },
    Stopped,
}
```

- [ ] **Step 2: Make `sync_env` two-way**

Change the `result` block's body:

```rust
    let result: Result<u64> = block_on(async {
        rdc::cli::init::write_scaffold_files(&folder, &env, &api_base, org_id)?;
        let token = rdc::secrets::resolve_token(&folder, &env, &api_base).await?;
        rdc::cli::sync::embed::sync_logged(
            &folder,
            &env,
            &token,
            rdc::cli::sync::embed::EmbedSyncOptions::default(),
            Box::new(forwarder),
        )
        .await?;
        Ok(discover::count_files(&folder.join(format!("envs/{env}"))))
    });
```

Update the doc comment on `sync_env`: it is no longer "pull-only", and the sentence about `sync_no_push` must go.

Note: a one-shot `sync_env` has **no prompt route installed**, so a gate it hits falls through to `read_line_coordinated`'s stdin branch and reads a GUI process's stdin — which is EOF. That degrades to `Skip`/`N`, which is safe but silent. Task 11 installs a route for `sync_env` too, using the same registry key, so a one-shot sync prompts exactly like a watch does. Leave a `// TODO(task-11)` comment here so the gap is not forgotten, and delete it in Task 11.

- [ ] **Step 3: Regenerate and fix Dart exhaustiveness**

Run:
```bash
flutter_rust_bridge_codegen --version   # must print 2.12.0
cd desktop && flutter_rust_bridge_codegen generate
```

`app_state.dart`'s `syncEnvItem` has a `switch (phase)` over the sealed class. Add the new arms; for now the four new ones are inert in a one-shot sync:

```dart
          case SyncPhase_Prompt():
          case SyncPhase_PromptResolved():
          case SyncPhase_Idle():
          case SyncPhase_Stopped():
            break; // handled by the watch stream (see watchEnvItem)
```

- [ ] **Step 4: Build both sides**

Run:
```bash
cd desktop/rust && cargo test 2>&1 | tail -20
cd desktop && flutter analyze
```
Expected: both clean.

- [ ] **Step 5: Commit**

```bash
git add desktop/rust/src/api/rdc.rs desktop/lib/src/rust desktop/rust/src/frb_generated.rs desktop/lib/src/app_state.dart
git commit -m "feat(desktop): sync_env is two-way, and SyncPhase can carry a prompt

Idle exists because rdc's own countdown is drawn with an in-place status
line, which is a verified no-op off a TTY and so never reaches a sink; the
app renders its own from this event.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 10: Bridge — the watch registry, `watch_env`, `stop_watch`

**Files:**
- Create: `desktop/rust/src/watch_registry.rs`
- Modify: `desktop/rust/src/lib.rs`
- Modify: `desktop/rust/src/api/rdc.rs`

**Interfaces:**
- Produces, in `crate::watch_registry`:
  - `pub struct WatchHandle { pub cancel: CancelToken, pub answers: std::sync::mpsc::Sender<String>, pub next_prompt_id: Arc<AtomicU64> }`
  - `pub fn insert(folder: &str, env: &str, h: WatchHandle) -> Option<WatchHandle>`
  - `pub fn get(folder: &str, env: &str) -> Option<WatchHandle>`
  - `pub fn remove(folder: &str, env: &str) -> Option<WatchHandle>`
- And in the bridge: `pub fn watch_env(folder, env, api_base, org_id, poll_secs: Option<u64>, sink) -> Result<()>`, `pub fn stop_watch(folder: String, env: String) -> Result<()>`.

- [ ] **Step 1: Write the registry**

Create `desktop/rust/src/watch_registry.rs`:

```rust
//! Live watches, keyed by `(folder, env)` — the same identity the Dart side
//! uses for its per-env state.
//!
//! Its own module rather than statics in `api::rdc` so the FFI surface stays
//! a thin translation layer: `flutter_rust_bridge_codegen` scans
//! `crate::api`, and anything public it finds there it tries to bridge.

use rdc::cli::sync::watch::CancelToken;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone)]
pub struct WatchHandle {
    pub cancel: CancelToken,
    /// Answers from the UI, delivered to whichever prompt is blocked.
    pub answers: Sender<String>,
    pub next_prompt_id: Arc<AtomicU64>,
}

type Map = HashMap<(String, String), WatchHandle>;

fn registry() -> &'static Mutex<Map> {
    static R: OnceLock<Mutex<Map>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Insert, returning any handle that was already there (which the caller
/// must cancel — two watches on one env would fight over the env lock).
pub fn insert(folder: &str, env: &str, h: WatchHandle) -> Option<WatchHandle> {
    registry()
        .lock()
        .unwrap()
        .insert((folder.to_string(), env.to_string()), h)
}

pub fn get(folder: &str, env: &str) -> Option<WatchHandle> {
    registry()
        .lock()
        .unwrap()
        .get(&(folder.to_string(), env.to_string()))
        .cloned()
}

pub fn remove(folder: &str, env: &str) -> Option<WatchHandle> {
    registry()
        .lock()
        .unwrap()
        .remove(&(folder.to_string(), env.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> WatchHandle {
        let (tx, _rx) = std::sync::mpsc::channel();
        WatchHandle {
            cancel: CancelToken::new(),
            answers: tx,
            next_prompt_id: Arc::new(AtomicU64::new(1)),
        }
    }

    #[test]
    fn insert_returns_the_displaced_handle() {
        let first = handle();
        assert!(insert("/tmp/acme", "dev", first.clone()).is_none());
        let displaced = insert("/tmp/acme", "dev", handle()).expect("should displace");
        displaced.cancel.cancel();
        assert!(displaced.cancel.is_cancelled());
        remove("/tmp/acme", "dev");
    }

    #[test]
    fn entries_are_scoped_per_folder_and_env() {
        insert("/tmp/acme", "dev", handle());
        assert!(get("/tmp/acme", "dev").is_some());
        assert!(get("/tmp/acme", "prod").is_none());
        assert!(get("/tmp/beta", "dev").is_none());
        remove("/tmp/acme", "dev");
    }
}
```

Add `mod watch_registry;` to `desktop/rust/src/lib.rs`.

- [ ] **Step 2: Add `watch_env` and `stop_watch` to the bridge**

In `desktop/rust/src/api/rdc.rs`, in a new `// ---- watch` section:

```rust
/// Watch one environment: reconcile once, then re-reconcile on a local file
/// change or on the poll timer, until `stop_watch` is called.
///
/// Blocks the calling FRB pool thread for the watch's whole life (the pool
/// is `num_cpus::get()` threads, so a great many concurrent watches would
/// starve other bridge calls). Progress, prompts and the between-cycle
/// countdown all arrive on `sink`.
///
/// Returns `Ok(())` even when the watch fails: the terminal outcome reaches
/// the caller as `SyncPhase::Error` / `SyncPhase::Stopped`, matching
/// `sync_env`'s contract.
pub fn watch_env(
    folder: String,
    env: String,
    api_base: String,
    org_id: u64,
    poll_secs: Option<u64>,
    sink: StreamSink<SyncPhase>,
) -> Result<()> {
    let folder_path = PathBuf::from(&folder);
    let _ = sink.add(SyncPhase::Started);

    let cancel = rdc::cli::sync::watch::CancelToken::new();
    let (answer_tx, answer_rx) = std::sync::mpsc::channel::<String>();
    let handle = crate::watch_registry::WatchHandle {
        cancel: cancel.clone(),
        answers: answer_tx,
        next_prompt_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
    };
    // One watch per env: a second would fight the first for the env lock.
    if let Some(previous) = crate::watch_registry::insert(&folder, &env, handle.clone()) {
        previous.cancel.cancel();
    }

    let route = std::sync::Arc::new(SinkPromptRoute {
        sink: sink.clone(),
        answers: std::sync::Mutex::new(answer_rx),
        next_id: handle.next_prompt_id.clone(),
    });

    let forwarder = LineForwarder { sink: sink.clone(), buf: Vec::new() };
    let poll = poll_secs.map(std::time::Duration::from_secs);

    let result: Result<()> = block_on(async {
        rdc::cli::init::write_scaffold_files(&folder_path, &env, &api_base, org_id)?;
        let token = rdc::secrets::resolve_token(&folder_path, &env, &api_base).await?;
        rdc::cli::sync::embed::watch_logged(
            &folder_path,
            &env,
            &api_base,
            token,
            poll,
            Box::new(forwarder),
            route,
            cancel,
        )
        .await
    });

    crate::watch_registry::remove(&folder, &env);
    match result {
        Ok(()) => {
            let _ = sink.add(SyncPhase::Stopped);
        }
        Err(e) => {
            let _ = sink.add(SyncPhase::Error { message: format!("{e:#}") });
        }
    }
    Ok(())
}

/// Ask a running watch to stop. No-op if that env is not being watched.
pub fn stop_watch(folder: String, env: String) -> Result<()> {
    if let Some(h) = crate::watch_registry::get(&folder, &env) {
        h.cancel.cancel();
    }
    Ok(())
}
```

`SinkPromptRoute` is Task 11's; add a `struct SinkPromptRoute;` stub with an `unimplemented!()` `ask` **only if** you need this task to compile standalone — otherwise do Tasks 10 and 11 as one commit. Prefer one commit: the two halves are meaningless apart.

- [ ] **Step 3: Build the registry tests**

Run:
```bash
cd desktop/rust && cargo test watch_registry 2>&1 | tail -10
```
Expected: both tests pass.

- [ ] **Step 4: Commit (with Task 11, if you combined them)**

Deferred to Task 11.

---

### Task 11: Bridge — `answer_prompt` and the sink-backed prompt route

**Files:**
- Modify: `desktop/rust/src/api/rdc.rs`
- Regenerate: `desktop/lib/src/rust/**`, `desktop/rust/src/frb_generated.rs`

**Interfaces:**
- Produces: `pub fn answer_prompt(folder: String, env: String, answer: String) -> Result<()>` and the `SinkPromptRoute` that implements `rdc::cli::stdin_coord::PromptRoute`.

**Why no `prompt_id` parameter:** a watch has at most one blocked prompt at a time (its cycle runs on one thread and blocks in `ask`). The `id` on the `Prompt` phase exists so the Dart side can *close the right dialog* when `PromptResolved` arrives — it is not needed to route the answer. Sending it back would invite a stale answer being accepted for a new prompt; instead a stale answer is impossible because the channel is drained before each ask.

- [ ] **Step 1: Implement the route**

```rust
/// Turns a blocked core prompt into a `SyncPhase::Prompt` on the stream and
/// blocks until the UI answers through `answer_prompt`.
///
/// `[e]` (shells out to $EDITOR) and `[h]` (a stateful per-hunk walk) are
/// stripped from the offered keys: neither has a meaning in a GUI process.
/// The core's own re-prompt loop covers the case where an answer arrives
/// that is not in the offered set.
struct SinkPromptRoute {
    sink: StreamSink<SyncPhase>,
    answers: std::sync::Mutex<std::sync::mpsc::Receiver<String>>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl rdc::cli::stdin_coord::PromptRoute for SinkPromptRoute {
    fn ask(&self, prompt: &rdc::cli::stdin_coord::Prompt) -> Option<String> {
        use std::sync::atomic::Ordering;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let keys: Vec<PromptChoice> = prompt
            .keys
            .iter()
            .filter(|k| !matches!(k.key, 'e' | 'h'))
            .map(|k| PromptChoice { key: k.key.to_string(), label: k.label.clone() })
            .collect();

        let rx = self.answers.lock().unwrap();
        // Drop anything queued from a previous prompt so a late answer can
        // never be read as the answer to this one.
        while rx.try_recv().is_ok() {}

        if self
            .sink
            .add(SyncPhase::Prompt {
                id,
                kind: kind_to_dto(prompt.kind),
                question: prompt.question.clone(),
                keys,
            })
            .is_err()
        {
            return None; // Dart stream gone: degrade to EOF (skip / N).
        }

        let answer = rx.recv().ok();
        let _ = self.sink.add(SyncPhase::PromptResolved { id });
        answer
    }
}

fn kind_to_dto(k: rdc::cli::stdin_coord::PromptKind) -> PromptKindDto {
    use rdc::cli::stdin_coord::PromptKind as K;
    match k {
        K::Conflict => PromptKindDto::Conflict,
        K::RemoteDelete => PromptKindDto::RemoteDelete,
        K::PushDrift => PromptKindDto::PushDrift,
        K::BulkConfirm => PromptKindDto::BulkConfirm,
        K::DeleteGate => PromptKindDto::DeleteGate,
        K::DeleteDrift => PromptKindDto::DeleteDrift,
        K::MdhIndexDrop => PromptKindDto::MdhIndexDrop,
        K::MdhRowDelete => PromptKindDto::MdhRowDelete,
    }
}

/// Answer the prompt a watch (or a one-shot sync) is currently blocked on.
/// No-op if nothing on that env is waiting — an answer for a prompt that
/// has already been torn down is dropped, not queued.
pub fn answer_prompt(folder: String, env: String, answer: String) -> Result<()> {
    if let Some(h) = crate::watch_registry::get(&folder, &env) {
        let _ = h.answers.send(answer);
    }
    Ok(())
}
```

Note `SinkPromptRoute` must be `Send + Sync`: `StreamSink` is `Send + Sync`, `Mutex<Receiver<String>>` is `Send + Sync` because `Receiver<String>` is `Send`. If the compiler disagrees, do not reach for `unsafe` — wrap the receiver differently and say why in a comment.

- [ ] **Step 2: Give a one-shot `sync_env` a route too**

`sync_env` currently installs nothing, so a gate it hits reads EOF and silently skips. Register a handle for the duration of the call and install the same route, then delete the `// TODO(task-11)` comment from Task 9:

```rust
    let cancel = rdc::cli::sync::watch::CancelToken::new();
    let (answer_tx, answer_rx) = std::sync::mpsc::channel::<String>();
    let next_id = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    // Registered so `answer_prompt` can find this cycle. A watch on the same
    // env would contend for the env lock anyway, so displacing one here is
    // the same rule `watch_env` applies.
    if let Some(previous) = crate::watch_registry::insert(
        &folder,
        &env,
        crate::watch_registry::WatchHandle {
            cancel: cancel.clone(),
            answers: answer_tx,
            next_prompt_id: next_id.clone(),
        },
    ) {
        previous.cancel.cancel();
    }
    let route: std::sync::Arc<dyn rdc::cli::stdin_coord::PromptRoute> =
        std::sync::Arc::new(SinkPromptRoute {
            sink: sink.clone(),
            answers: std::sync::Mutex::new(answer_rx),
            next_id,
        });
    let _route_guard = rdc::cli::stdin_coord::install_route(route);
```

The guard must live for the whole `block_on`, and `install_route` must be called **on the same thread** that runs the cycle — which it is, because `block_on` runs the future on this thread. Call `crate::watch_registry::remove(&folder, &env);` before returning.

- [ ] **Step 3: Regenerate and check the surface**

Run:
```bash
flutter_rust_bridge_codegen --version   # 2.12.0
cd desktop && flutter_rust_bridge_codegen generate
grep -n "watchEnv\|stopWatch\|answerPrompt\|SyncPhase_Prompt" lib/src/rust/api/rdc.dart | head
```
Expected: the three functions and the `SyncPhase_Prompt` variant appear in the generated Dart. `SinkPromptRoute`, `kind_to_dto` and the registry must **not** appear — they are private or outside `crate::api`.

- [ ] **Step 4: Build and test**

Run:
```bash
cd desktop/rust && cargo test 2>&1 | tail -20
cd desktop && flutter analyze
```
Expected: clean. `flutter analyze` will flag the non-exhaustive switch in `app_state.dart` only if Task 9's placeholder arms were removed; they were not.

- [ ] **Step 5: Commit**

```bash
git add desktop/rust/src/watch_registry.rs desktop/rust/src/lib.rs desktop/rust/src/api/rdc.rs desktop/lib/src/rust desktop/rust/src/frb_generated.rs
git commit -m "feat(desktop): watch_env, stop_watch and answer_prompt

A blocked core prompt becomes a SyncPhase::Prompt and blocks on a channel
the UI feeds. [e] and [h] are stripped from the offered keys — one shells
out to \$EDITOR, the other is a per-hunk terminal walk.

A one-shot sync_env installs the same route, so a delete gate it hits is a
dialog rather than an EOF that silently skips.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 12: Dart — watch state

**Files:**
- Create: `desktop/lib/src/watch_state.dart`
- Modify: `desktop/lib/src/app_state.dart`
- Create: `desktop/test/watch_state_test.dart`

**Interfaces:**
- Produces:
  - `class PendingPrompt { final BigInt id; final PromptKindDto kind; final String question; final List<PromptChoice> keys; final String folder; final String env; }`
  - `class WatchState { bool running; int? nextPollSecs; PendingPrompt? prompt; }`
  - On `AppState`: `Map<String, WatchState> watch`, `void watchEnvItem(ProjectItem, EnvSummary)`, `void stopWatchItem(ProjectItem, EnvSummary)`, `void answer(PendingPrompt, String key)`, `List<PendingPrompt> get promptQueue`, `bool isWatching(String folder, String env)`.

- [ ] **Step 1: Write the state file**

Create `desktop/lib/src/watch_state.dart`:

```dart
import 'rust/api/rdc.dart';

/// A prompt a cycle is blocked on, tagged with the env it came from so the
/// dialog can name it and the answer can be routed back.
class PendingPrompt {
  const PendingPrompt({
    required this.id,
    required this.kind,
    required this.question,
    required this.keys,
    required this.folder,
    required this.env,
  });

  final BigInt id;
  final PromptKindDto kind;
  final String question;
  final List<PromptChoice> keys;
  final String folder;
  final String env;

  /// Dialog title. The question line itself is shown verbatim underneath.
  String get title => switch (kind) {
        PromptKindDto.conflict => 'Changed in both places',
        PromptKindDto.remoteDelete => 'Deleted on one side',
        PromptKindDto.pushDrift => 'Changed remotely while syncing',
        PromptKindDto.bulkConfirm => 'Apply to all?',
        PromptKindDto.deleteGate => 'Delete from Rossum?',
        PromptKindDto.deleteDrift => 'Deleted locally, changed remotely',
        PromptKindDto.mdhIndexDrop => 'Drop indexes?',
        PromptKindDto.mdhRowDelete => 'Delete rows?',
      };
}

/// Per-env watch state. Absent from `AppState.watch` means "not watching".
class WatchState {
  WatchState({this.running = false, this.nextPollSecs, this.prompt});
  bool running;
  int? nextPollSecs;
  PendingPrompt? prompt;
}
```

- [ ] **Step 2: Wire it into `AppState`**

Add to `AppState`:

```dart
  /// Live watches, keyed like [syncState] by (folder, env).
  final Map<String, WatchState> watch = {};

  bool isWatching(String folder, String env) =>
      watch[envKey(folder, env)]?.running ?? false;

  /// Every prompt currently blocking, oldest first. More than one is
  /// reachable: two watched envs can block at the same time.
  List<PendingPrompt> get promptQueue =>
      [for (final w in watch.values) if (w.prompt != null) w.prompt!];

  void watchEnvItem(ProjectItem item, EnvSummary env) {
    final folder = item.summary.folder;
    final k = envKey(folder, env.name);
    if (watch[k]?.running ?? false) return; // already watching
    watch[k] = WatchState(running: true);
    syncLog[k] = <String>[];
    notifyListeners();

    watchEnv(
      folder: folder,
      env: env.name,
      apiBase: env.apiBase,
      orgId: env.orgId,
      // Task 15 replaces this with the per-env setting.
      pollSecs: BigInt.from(60),
    ).listen(
      (phase) {
        final w = watch[k];
        if (w == null) return; // stopped and cleared while in flight
        switch (phase) {
          case SyncPhase_Started():
            w.running = true;
          case SyncPhase_Log(:final line):
            (syncLog[k] ??= <String>[]).add(line);
            w.nextPollSecs = null; // a cycle is running
          case SyncPhase_Prompt(:final id, :final kind, :final question, :final keys):
            w.prompt = PendingPrompt(
              id: id, kind: kind, question: question, keys: keys,
              folder: folder, env: env.name,
            );
          case SyncPhase_PromptResolved(:final id):
            if (w.prompt?.id == id) w.prompt = null;
          case SyncPhase_Idle(:final nextPollSecs):
            w.nextPollSecs = nextPollSecs?.toInt();
          case SyncPhase_Done():
            reload();
          case SyncPhase_Error(:final message):
            w.running = false;
            w.prompt = null;
            syncState[k] = SyncState.error;
            syncMessage[k] = message;
          case SyncPhase_Stopped():
            watch.remove(k);
            reload();
        }
        notifyListeners();
      },
      onError: (Object e) {
        watch.remove(k);
        syncState[k] = SyncState.error;
        syncMessage[k] = errorText(e);
        notifyListeners();
      },
    );
  }

  void stopWatchItem(ProjectItem item, EnvSummary env) {
    stopWatch(folder: item.summary.folder, env: env.name);
    // The registry cancels; SyncPhase_Stopped clears the entry. Mark it
    // stopping now so the button flips immediately.
    watch[envKey(item.summary.folder, env.name)]?.running = false;
    notifyListeners();
  }

  void answer(PendingPrompt p, String key) {
    answerPrompt(folder: p.folder, env: p.env, answer: key);
    watch[envKey(p.folder, p.env)]?.prompt = null;
    notifyListeners();
  }
```

Check the generated `watchEnv`'s actual `pollSecs` parameter type before compiling: the Rust side is `Option<u64>`, which FRB renders as `BigInt?`. If the generated signature differs, match it rather than changing the Rust.

- [ ] **Step 3: Write the tests**

Create `desktop/test/watch_state_test.dart`:

```dart
import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:desktop/src/watch_state.dart';
import 'package:flutter_test/flutter_test.dart';

PendingPrompt _prompt(String folder, String env, int id) => PendingPrompt(
      id: BigInt.from(id),
      kind: PromptKindDto.deleteGate,
      question: 'Proceed with deletion? [y/N] ',
      keys: [PromptChoice(key: 'y', label: 'delete them'),
                   PromptChoice(key: 'n', label: 'cancel')],
      folder: folder,
      env: env,
    );

void main() {
  test('watch state is keyed per (folder, env)', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    s.watch[s.envKey('/tmp/acme', 'dev')] = WatchState(running: true);
    expect(s.isWatching('/tmp/acme', 'dev'), isTrue);
    expect(s.isWatching('/tmp/acme', 'prod'), isFalse);
    expect(s.isWatching('/tmp/beta', 'dev'), isFalse);
  });

  test('two blocked envs both appear in the prompt queue', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    s.watch[s.envKey('/tmp/acme', 'dev')] =
        WatchState(running: true, prompt: _prompt('/tmp/acme', 'dev', 1));
    s.watch[s.envKey('/tmp/beta', 'dev')] =
        WatchState(running: true, prompt: _prompt('/tmp/beta', 'dev', 1));
    expect(s.promptQueue.length, 2);
  });

  test('a stale PromptResolved does not clear a newer prompt', () {
    final w = WatchState(running: true, prompt: _prompt('/tmp/acme', 'dev', 7));
    // Simulating the guard in the SyncPhase_PromptResolved arm.
    if (w.prompt?.id == BigInt.from(6)) w.prompt = null;
    expect(w.prompt, isNotNull);
    if (w.prompt?.id == BigInt.from(7)) w.prompt = null;
    expect(w.prompt, isNull);
  });

  test('every prompt kind has a title', () {
    for (final k in PromptKindDto.values) {
      final t = _prompt('/tmp/acme', 'dev', 1);
      expect(PendingPrompt(id: t.id, kind: k, question: t.question,
              keys: t.keys, folder: t.folder, env: t.env)
          .title
          .isNotEmpty, isTrue, reason: 'no title for $k');
    }
  });
}
```

- [ ] **Step 4: Run**

Run:
```bash
cd desktop && flutter test test/watch_state_test.dart && flutter analyze
```
Expected: four tests pass, analyze clean.

- [ ] **Step 5: Commit**

```bash
git add desktop/lib/src/watch_state.dart desktop/lib/src/app_state.dart desktop/test/watch_state_test.dart
git commit -m "feat(desktop): per-env watch state and the prompt queue

Two watched envs can block at the same time, so a pending prompt belongs to
an env rather than to the app, and PromptResolved is matched by id so a
stale one cannot close a newer dialog.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 13: Dart — the prompt dialog

**Files:**
- Modify: `desktop/lib/src/dialogs.dart`
- Create: `desktop/test/prompt_dialog_test.dart`

**Interfaces:**
- Produces: `class PromptDialog extends StatelessWidget { const PromptDialog({required this.prompt, required this.logTail, required this.onAnswer}); }` where `onAnswer` is `void Function(String key)`.

- [ ] **Step 1: Read the idiom you are about to follow**

Run:
```bash
cd desktop && sed -n '12,66p' lib/src/dialogs.dart && sed -n '431,470p' lib/src/dialogs.dart
```

That is `_Frame`'s constructor and `RemoveDialog`, the simplest dialog in the file. The widget below assumes `_Frame({required String title, required List<Widget> children})`; if the real constructor differs, **match the real one** — do not add a second frame widget. Likewise `_mono`, `_Btn` and `ansiSpans`: `ansiSpans` is exported from `ansi.dart`, but `_mono` and `_Btn` are private to `home_page.dart`. If `dialogs.dart` already has equivalents, use those; if not, lift the two out of `home_page.dart` into `mdh_theme.dart` and import them in both places. Do not duplicate them.

- [ ] **Step 2: Write the widget**

Append to `desktop/lib/src/dialogs.dart`:

```dart
/// A blocked cycle, rendered. The body is the tail of the sync log — which
/// is where the diff, the connector line and the object list already are,
/// in colour — and the buttons are exactly the keys the core offered.
class PromptDialog extends StatelessWidget {
  const PromptDialog({
    super.key,
    required this.prompt,
    required this.logTail,
    required this.onAnswer,
  });

  final PendingPrompt prompt;
  final List<String> logTail;
  final void Function(String key) onAnswer;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final spans = <InlineSpan>[];
    for (var i = 0; i < logTail.length; i++) {
      spans.addAll(ansiSpans(logTail[i], c, 12.5));
      if (i < logTail.length - 1) spans.add(const TextSpan(text: '\n'));
    }
    return _Frame(
      title: '${prompt.title} · ${prompt.env}',
      children: [
        Container(
          width: double.infinity,
          constraints: const BoxConstraints(maxHeight: 260),
          decoration: BoxDecoration(
            color: c.bgCode,
            border: Border.all(color: c.borderCard),
            borderRadius: BorderRadius.circular(6),
          ),
          padding: const EdgeInsets.fromLTRB(14, 12, 14, 12),
          child: SingleChildScrollView(
            child: SelectableText.rich(TextSpan(children: spans)),
          ),
        ),
        const SizedBox(height: 12),
        SelectableText(prompt.question, style: _mono(c.textSecondary, 12.5)),
        const SizedBox(height: 12),
        Wrap(
          spacing: 8,
          runSpacing: 8,
          children: [
            for (final k in prompt.keys)
              _Btn(
                label: '[${k.key}] ${k.label}',
                primary: k.key == 'n' || k.key == 's',
                onTap: () => onAnswer(k.key),
              ),
          ],
        ),
      ],
    );
  }
}
```

The safe default is highlighted, not the destructive one: `[n]` (cancel) and `[s]` (skip) are the keys that change nothing.

- [ ] **Step 3: Write the widget test**

Create `desktop/test/prompt_dialog_test.dart`:

```dart
import 'package:desktop/src/dialogs.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/watch_state.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

PendingPrompt _conflict() => PendingPrompt(
      id: BigInt.one,
      kind: PromptKindDto.conflict,
      question: '[k] keep local  [r] use dev  [s] skip (shadow file)  [a] abort > ',
      // What the bridge offers after stripping [e] and [h].
      keys: [
        PromptChoice(key: 'k', label: 'keep local'),
        PromptChoice(key: 'r', label: 'use dev'),
        PromptChoice(key: 's', label: 'skip (shadow file)'),
        PromptChoice(key: 'a', label: 'abort'),
      ],
      folder: '/tmp/acme',
      env: 'dev',
    );

void main() {
  testWidgets('offers one button per key and reports the key pressed', (t) async {
    String? answered;
    await t.pumpWidget(MaterialApp(
      theme: mdhTheme(Brightness.light),
      home: Scaffold(
        body: PromptDialog(
          prompt: _conflict(),
          logTail: ['patch  queues  invoices  +2  -1'],
          onAnswer: (k) => answered = k,
        ),
      ),
    ));

    expect(find.textContaining('[k] keep local'), findsOneWidget);
    expect(find.textContaining('[r] use dev'), findsOneWidget);
    // The two terminal-only keys must never reach the UI.
    expect(find.textContaining('[e]'), findsNothing);
    expect(find.textContaining('[h]'), findsNothing);

    await t.tap(find.textContaining('[r] use dev'));
    await t.pump();
    expect(answered, 'r');
  });
}
```

`mdhTheme` may be named differently — check `mdh_theme.dart` and use the real name.

- [ ] **Step 4: Run**

Run:
```bash
cd desktop && flutter test test/prompt_dialog_test.dart && flutter analyze
```
Expected: passes, analyze clean.

- [ ] **Step 5: Commit**

```bash
git add desktop/lib/src/dialogs.dart desktop/test/prompt_dialog_test.dart
git commit -m "feat(desktop): PromptDialog renders a blocked cycle

The body is the log tail, which already carries the diff and the object
list in colour now that prompt output goes through the Log. Buttons are
exactly the keys the core offered, and the safe key is the highlighted one.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 14: Dart — the watch UI surfaces

**Files:**
- Modify: `desktop/lib/src/home_page.dart`

**Interfaces:**
- Consumes: Tasks 12 and 13.
- Produces: `_St` gains `watching`; `_ConnBar` gains a Watch/Stop button; `_EnvTableRow` gains a watch icon; `_EnvRow` shows the countdown; `_HomePageState` shows `PromptDialog` for the head of `state.promptQueue`.

- [ ] **Step 1: Extend the status enum and its two renderers**

```dart
enum _St { running, watching, error, synced, never }

_St _statusOf(AppState s, ProjectItem it, EnvSummary env) {
  switch (s.syncState[s.envKey(it.summary.folder, env.name)]) {
    case SyncState.running:
      return _St.running;
    case SyncState.error:
      return _St.error;
    default:
      if (s.isWatching(it.summary.folder, env.name)) return _St.watching;
      return env.lastSyncUnix != null ? _St.synced : _St.never;
  }
}
```

`error` still outranks `watching` — a failed cycle is the more urgent fact.

```dart
(String, Color, Color) _badgeFor(MdhColors c, _St st) => switch (st) {
      _St.error => ('error', c.dangerBg, c.dangerFg),
      _St.never => ('never', c.infoBg, c.infoFg),
      _St.running => ('syncing', c.infoBg, c.infoFg),
      _St.watching => ('watching', c.infoBg, c.accent),
      _St.synced => ('synced', c.successBg, c.successFg),
    };
```

Every other `switch (st)` in the file becomes non-exhaustive — `flutter analyze` will list them. Fix each: `_SyncLogCard`'s status line gets `_St.watching => ('watching…', c.textPrimary)`, and `_EnvRow`'s `sub` gets the countdown (Step 3).

- [ ] **Step 2: The header and row buttons**

In `_ConnBar`, after the Sync button:

```dart
          const SizedBox(width: 8),
          _Btn(
            label: state.isWatching(item.summary.folder, env.name) ? 'Stop' : 'Watch',
            onTap: () => state.isWatching(item.summary.folder, env.name)
                ? state.stopWatchItem(item, env)
                : state.watchEnvItem(item, env),
          ),
```

`_ConnBar` takes `state` already. In `_EnvTableRow`'s action cluster, between Sync and Edit:

```dart
              const SizedBox(width: 6),
              _RowIconBtn(
                icon: state.isWatching(item.summary.folder, env.name)
                    ? Icons.visibility
                    : Icons.visibility_outlined,
                tooltip: state.isWatching(item.summary.folder, env.name) ? 'Stop watching' : 'Watch',
                onTap: () => state.isWatching(item.summary.folder, env.name)
                    ? state.stopWatchItem(item, env)
                    : state.watchEnvItem(item, env),
              ),
```

- [ ] **Step 3: The sidebar countdown**

In `_EnvRow`:

```dart
    final w = state.watch[state.envKey(item.summary.folder, env.name)];
    final sub = switch (st) {
      _St.running => 'syncing…',
      _St.watching => w?.nextPollSecs != null ? 'watching · ${w!.nextPollSecs}s' : 'watching',
      _St.error => 'failed',
      _St.synced => _rel(env.lastSyncUnix),
      _St.never => 'never',
    };
```

and give the dot the watching colour:

```dart
    final dotColor = switch (st) {
      _St.error => c.danger,
      _St.never => c.textHint,
      _St.watching => c.accent,
      _ => c.successFg,
    };
```

- [ ] **Step 4: Show the dialog**

In `_HomePageState.build`, wrap the returned scaffold so a pending prompt overlays it. Follow whatever pattern `_HomePageState` already uses for its update-check dialog (`update_check.dart` is wired in there); if that uses `showDialog` from a listener, do the same — a prompt arriving while a dialog is open must not stack a second one:

```dart
    // One dialog at a time, oldest first. A second env blocking while this
    // is open waits its turn; its row still shows the watching badge.
    final pending = state.promptQueue;
    if (pending.isNotEmpty && !_promptOpen) {
      _promptOpen = true;
      final p = pending.first;
      WidgetsBinding.instance.addPostFrameCallback((_) async {
        await showDialog<void>(
          context: context,
          barrierDismissible: false, // a cycle is blocked; there is no "later"
          builder: (_) => PromptDialog(
            prompt: p,
            logTail: (state.syncLog[state.envKey(p.folder, p.env)] ?? const <String>[])
                .reversed
                .take(40)
                .toList()
                .reversed
                .toList(),
            onAnswer: (k) {
              state.answer(p, k);
              Navigator.of(context).pop();
            },
          ),
        );
        _promptOpen = false;
      });
    }
```

`barrierDismissible: false` because the cycle is genuinely blocked — dismissing would leave it waiting with no way to answer.

- [ ] **Step 5: Run everything**

Run:
```bash
cd desktop && flutter analyze && flutter test
```
Expected: analyze clean. `golden_mdh_test.dart` may fail if any seeded state now renders a watch badge — it should not, since `_seeded()` sets no watch state. If a golden does move, stop and find out why before regenerating.

- [ ] **Step 6: Commit**

```bash
git add desktop/lib/src/home_page.dart
git commit -m "feat(desktop): Watch button, watching badge, poll countdown, prompt dialog

The countdown comes from SyncPhase.Idle rather than rdc's own status line,
which is drawn in place and is a no-op off a TTY.

One dialog at a time: a second env blocking waits its turn, and the barrier
is not dismissible because the cycle behind it is genuinely blocked.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 15: Poll interval and the one-time two-way notice

**Files:**
- Modify: `desktop/lib/src/settings.dart`
- Modify: `desktop/lib/src/app_state.dart`
- Modify: `desktop/lib/src/home_page.dart`
- Modify: `desktop/test/settings_forward_compat_test.dart`

**Interfaces:**
- Produces on `Settings`: `Map<String, dynamic> watch` (keyed `"<folder> <env>"` → `{"pollSecs": int}`), `List<String> ackTwoWay` (project folders), `int? pollSecsFor(String folder, String env)`, `void setPollSecs(String folder, String env, int? secs)`, `bool hasAckedTwoWay(String folder)`, `void ackTwoWayFor(String folder)`.

- [ ] **Step 1: Extend Settings**

The promote-removal plan left `Settings` with two owned keys and an `extra` map for everything else. Add two more owned keys — constructor parameter, initializer, `_known`, `fromJson` and `toJson` — so they stop being round-tripped blindly and start being read:

```dart
  static const _known = {'parentFolder', 'externalPaths', 'watch', 'ackTwoWay'};

  Settings({
    this.parentFolder,
    List<String>? externalPaths,
    Map<String, dynamic>? watch,
    List<String>? ackTwoWay,
    Map<String, dynamic>? extra,
    File? file,
  })  : externalPaths = externalPaths ?? [],
        watch = watch ?? {},
        ackTwoWay = ackTwoWay ?? [],
        extra = extra ?? {},
        _overrideFile = file;

  static Settings fromJson(Map<String, dynamic> m) => Settings(
        parentFolder: m['parentFolder'] as String?,
        externalPaths:
            (m['externalPaths'] as List?)?.map((e) => e as String).toList() ?? [],
        watch: (m['watch'] as Map?)?.cast<String, dynamic>() ?? {},
        ackTwoWay:
            (m['ackTwoWay'] as List?)?.map((e) => e as String).toList() ?? [],
        extra: {
          for (final e in m.entries)
            if (!_known.contains(e.key)) e.key: e.value,
        },
      );

  Map<String, dynamic> toJson() => {
        ...extra,
        'parentFolder': parentFolder,
        'externalPaths': externalPaths,
        'watch': watch,
        'ackTwoWay': ackTwoWay,
      };
```

Then the fields and accessors:

```dart
  /// Per-env watch options, keyed `<folder> <env>` like AppState's
  /// envKey. Only `pollSecs` today; absent means the 60s default.
  Map<String, dynamic> watch;

  /// Projects whose owner has seen the one-time notice that Sync now writes
  /// to Rossum. Per project, not per env — the surprise is about the app.
  List<String> ackTwoWay;

  int? pollSecsFor(String folder, String env) {
    final v = watch['$folder $env'];
    return v is Map ? v['pollSecs'] as int? : null;
  }

  void setPollSecs(String folder, String env, int? secs) {
    final k = '$folder $env';
    if (secs == null) {
      watch.remove(k);
    } else {
      watch[k] = {'pollSecs': secs};
    }
    save();
  }

  bool hasAckedTwoWay(String folder) => ackTwoWay.contains(folder);

  void ackTwoWayFor(String folder) {
    if (!ackTwoWay.contains(folder)) {
      ackTwoWay.add(folder);
      save();
    }
  }
```

- [ ] **Step 2: Use the setting in `watchEnvItem`**

Replace the hardcoded `pollSecs: 60` from Task 12:

```dart
      pollSecs: BigInt.from(_settings.pollSecsFor(folder, env.name) ?? 60),
```

Check the generated Dart's actual parameter type for `pollSecs` — it is `BigInt?` if the Rust side is `Option<u64>`. Match it.

- [ ] **Step 3: The one-time notice**

Add to `AppState`:

```dart
  /// True when this project's owner has not yet been told that Sync writes
  /// to Rossum. The app was pull-only until this release.
  bool needsTwoWayNotice(String folder) => !_settings.hasAckedTwoWay(folder);

  void ackTwoWay(String folder) {
    _settings.ackTwoWayFor(folder);
    notifyListeners();
  }
```

In `dialogs.dart`, next to `RemoveDialog` and following the same `_Frame` shape:

```dart
/// Shown once per project, the first time it is synced by a build whose
/// Sync writes. Every earlier build's Sync was `--no-push`, so a project
/// used as a read-only archive would otherwise start pushing with no
/// warning at all.
class TwoWayNoticeDialog extends StatelessWidget {
  const TwoWayNoticeDialog({super.key, required this.projectName});
  final String projectName;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return _Frame(
      title: 'Sync now writes to Rossum',
      children: [
        Text(
          'Syncing $projectName sends your local changes under envs/ to the Rossum '
          'organization, and pulls its changes back. Earlier versions of this app '
          'only pulled.\n\n'
          'Deletions and conflicts still stop and ask first.',
          style: TextStyle(color: c.textPrimary, fontSize: 12.5, height: 1.45),
        ),
        const SizedBox(height: 16),
        Row(mainAxisAlignment: MainAxisAlignment.end, children: [
          _Btn(label: 'Cancel', onTap: () => Navigator.of(context).pop(false)),
          const SizedBox(width: 8),
          _Btn(label: 'Sync', primary: true, onTap: () => Navigator.of(context).pop(true)),
        ]),
      ],
    );
  }
}
```

In `home_page.dart`, gate both the `onSync` and `onSyncAll` handlers on it:

```dart
  /// Returns true when the sync should proceed.
  Future<bool> _confirmTwoWay(AppState state, ProjectItem item) async {
    if (!state.needsTwoWayNotice(item.summary.folder)) return true;
    final ok = await showDialog<bool>(
          context: context,
          builder: (_) => TwoWayNoticeDialog(projectName: item.summary.name),
        ) ??
        false;
    if (ok) state.ackTwoWay(item.summary.folder);
    return ok;
  }
```

and call it before `state.syncEnvItem(item, env)` in both places — **and before `state.watchEnvItem(item, env)` too**. A watch's initial reconcile is a full two-way cycle, so pressing Watch on a project that has never been synced by this build would push without the notice ever appearing. That is exactly the case the notice exists for.

- [ ] **Step 4: Extend the forward-compat test**

In `desktop/test/settings_forward_compat_test.dart`, extend the first test's fixture with `'watch': {...}` and `'ackTwoWay': [...]` and assert both survive `toJson`, and add:

```dart
  test('pollSecs round-trips per env and defaults to null', () {
    final s = Settings.fromJson({});
    expect(s.pollSecsFor('/tmp/acme', 'dev'), isNull);
    s.watch['/tmp/acme dev'] = {'pollSecs': 300};
    expect(s.pollSecsFor('/tmp/acme', 'dev'), 300);
    expect(s.pollSecsFor('/tmp/acme', 'prod'), isNull);
  });
```

Note `setPollSecs` calls `save()`, so a test that calls it must construct `Settings(file: tmpFile)` with a temp file — copy that sandboxing from the (now deleted) `promote_defaults_test.dart` pattern, which the promote-removal plan preserved in `settings_forward_compat_test.dart`'s neighbours. Never let a test write the developer's real `~/.rdc-desktop/settings.json`.

- [ ] **Step 5: Run**

Run:
```bash
cd desktop && flutter test && flutter analyze
```
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add desktop/lib/src/settings.dart desktop/lib/src/app_state.dart desktop/lib/src/home_page.dart desktop/test/settings_forward_compat_test.dart
git commit -m "feat(desktop): per-env poll interval and a one-time two-way notice

Sync was --no-push until now, so a project that has been used as a
read-only archive would start writing with no warning. The notice fires
once per project and is recorded in settings.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 16: Goldens and documentation

**Files:**
- Modify: `desktop/test/golden_mdh_test.dart`
- Create: `desktop/test/goldens/mdh_watching_light.png`, `desktop/test/goldens/mdh_prompt_light.png`
- Modify: `desktop/README.md`
- Modify: `README.md`

- [ ] **Step 1: Add the two golden cases**

In `desktop/test/golden_mdh_test.dart`, add a seeded state with a watching env and a pending prompt, following `_seeded()`'s existing shape:

```dart
AppState _watchingState() {
  final s = _seeded();
  const sel = '/tmp/Rossum/acme-invoices-eu-prod-primary';
  s.watch[s.envKey(sel, 'prod')] = WatchState(running: true, nextPollSecs: 42);
  return s;
}

  testWidgets('watching env — light', (t) async {
    await shot(
        t,
        _wrap(Brightness.light,
            MdhScaffold(state: _watchingState(), view: NavView.connection, onSelectEnv: (f, e) {})),
        'goldens/mdh_watching_light.png');
  });

  testWidgets('prompt dialog — light', (t) async {
    await shot(
        t,
        _wrap(
            Brightness.light,
            PromptDialog(
              prompt: PendingPrompt(
                id: BigInt.one,
                kind: PromptKindDto.deleteGate,
                question: 'Proceed with deletion? [y/N] ',
                keys: [
                  PromptChoice(key: 'y', label: 'delete them'),
                  PromptChoice(key: 'n', label: 'cancel'),
                ],
                folder: '/tmp/Rossum/acme-invoices',
                env: 'prod',
              ),
              logTail: [
                '14:12:03 delete 2 object(s) would be DELETED from the remote',
                '         delete  queues          old-intake                id 41',
                '         delete  schemas         old-intake                id 77',
              ],
              onAnswer: (_) {},
            )),
        'goldens/mdh_prompt_light.png');
  });
```

Keep every string in the fixtures neutral — `acme`, `invoices`, `old-intake`, `dev`/`prod`. These files are committed.

- [ ] **Step 2: Shoot and inspect them**

Run:
```bash
cd desktop && flutter test --update-goldens test/golden_mdh_test.dart
cd /Users/martin.zlamal@rossum.ai/Work/github.com/mrtnzlml/rdc && git status --short desktop/test/goldens/
```
Expected: exactly two new PNGs, no existing one modified. If an existing golden moved, revert it and find out why.

Open both and check: the watching badge and `watching · 42s` are legible, the dialog's log tail is not clipped, and the two buttons fit.

- [ ] **Step 3: Update the desktop README**

Replace the Promote section's slot (removed by the prior plan) with:

```markdown
## Sync and watch

**Sync** runs one full `rdc sync <env>` cycle: local changes in `envs/<env>/`
go to the organization, its changes come back. Earlier versions of this app
only pulled; the first sync of a project says so once.

**Watch** keeps that cycle running — a local file change triggers one
immediately, and a timer polls the organization for drift (60s by default).
Several environments can be watched at once.

Anything destructive stops and asks: a conflict, a remote deletion, a pending
DELETE, an MDH index drop. Those are the same questions `rdc sync` asks in a
terminal, rendered as a dialog. The two terminal-only answers — `[e]` (open
`$EDITOR`) and `[h]` (walk the diff hunk by hunk) — are not offered here; use
the CLI for those.

The app never writes to an environment other than the one being synced.
Promoting configuration between environments is the GitLab CI deploy job's
work, where it is gated on the test suite.
```

- [ ] **Step 4: Document `--watch` in the root README**

`README.md` documents `rdc sync` and never mentions `--watch`. Add a subsection after **Preview a sync**:

```markdown
### Watch an environment

```sh
rdc sync test --watch
```

Reconciles once, then keeps going: a change under `envs/test/` triggers a
cycle, and a timer polls the environment for drift. `--poll-interval 5m`
changes the cadence, `--no-poll` turns polling off and leaves the file
watcher, and pressing Enter runs a cycle immediately. Conflicts and deletions
prompt exactly as they do in a one-shot sync; `--conflict` is not accepted
here, because a watch that resolves conflicts without asking would do so
unattended and repeatedly.
```

Verify each flag against `src/cli/mod.rs`'s `Sync` variant before writing it — the arg definitions are the source of truth, not this plan.

- [ ] **Step 5: Correct three stale doc comments in `src/` that this plan's predecessor could not touch**

The promote-removal plan froze `src/`, so three comments there still assert a desktop promote
path that no longer exists. They were found by that plan's whole-branch review and handed
forward, because nothing else in this plan touches `migrate` and they would otherwise persist
indefinitely.

`src/cli/sync/embed.rs` — the `sync_push_logged` doc says "Used by the desktop app's promote
Push." Task 8 of this plan deletes that function outright, so this one **self-resolves**; only
verify it is gone.

`src/cli/migrate/mod.rs`, on `run_at` (near line 2573) — documented as "the embedding seam
non-CLI consumers (e.g. the desktop app's promote flow) use". No non-CLI consumer remains;
`run_at`'s only caller is now `migrate::run`. The function is still needed — rewrite the
comment to describe what it is (the cwd-parameterised form of `migrate::run`) without claiming
a caller that does not exist.

`src/cli/migrate/mod.rs`, on `format_saved_view_ref_error` (near line 743) — its rationale
reads "the whole listing lives in the returned error rather than in `eprintln!`s beside it,
because `run_at` is also the desktop app's promote seam: an error written straight to stderr
never reaches a GUI." The behaviour is right and stays; the justification is dead. Keep the
first half (an error carrying its own listing is better than stderr side-effects) and drop the
promote-seam clause.

Verify the line numbers before editing — they are from a review at `5fe7b6a` and this plan has
changed `src/` since. Grep for `promote` under `src/` and fix what you find; there should be
nothing left after this step.

- [ ] **Step 6: Full verification**

Run:
```bash
cd /Users/martin.zlamal@rossum.ai/Work/github.com/mrtnzlml/rdc && cargo test 2>&1 | tail -5
cd desktop/rust && cargo test 2>&1 | tail -5
cd desktop && flutter analyze && flutter test && flutter build macos 2>&1 | tail -5
```
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add desktop/test/golden_mdh_test.dart desktop/test/goldens desktop/README.md README.md src/cli/migrate/mod.rs
git commit -m "docs: document sync-and-watch, and --watch in the root README

--watch shipped in 2026-05 and has never been in the README.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Done criteria

```bash
cargo test                     # core: green, prompt pins included
cd desktop/rust && cargo test  # bridge: green
cd desktop && flutter analyze  # No issues found!
cd desktop && flutter test     # green, goldens included
cd desktop && flutter build macos
```

Plus, by hand against a real sandbox org, since none of the above proves the
loop actually runs:

1. Watch an env. Confirm the sidebar countdown ticks and resets after a cycle.
2. Edit a file under `envs/<env>/`. Confirm a cycle fires within ~1s and the
   change lands in the organization.
3. Delete a file under `envs/<env>/`. Confirm the delete gate appears as a
   dialog listing the objects, that answering `n` leaves the organization
   untouched, and — critically — **that the watch is still running afterwards**.
   Before this change that path was a `bail!` that killed the loop.
4. Change the same object in the Rossum UI and locally, then trigger a cycle.
   Confirm the conflict dialog shows the actual diff.
5. Stop the watch. Confirm the row returns to `synced` and no thread is left
   spinning.
