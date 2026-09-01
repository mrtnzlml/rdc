//! `rdc sync --watch <env>` — foreground watch mode.
//!
//! Spec: docs/superpowers/specs/2026-05-14-watch-mode-design.md

use anyhow::Result;
use futures::future::BoxFuture;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
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
    ///
    /// Uses tokio's documented enable-then-check pattern rather than a bare
    /// `notified().await`: `enable()` registers this waiter *before* we
    /// check the flag, so a `cancel()` on another thread landing between
    /// the check and the await is still delivered as a wakeup instead of
    /// being missed (a `Notify::notify_waiters` stores no permit for a
    /// waiter that subscribes after it fires).
    pub async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// Ten-segment bar showing position within the polling interval.
fn polling_bar(elapsed: u64, total: u64) -> String {
    const SEGMENTS: u64 = 10;
    let filled = elapsed
        .saturating_mul(SEGMENTS)
        .checked_div(total)
        .map_or(SEGMENTS, |f| f.min(SEGMENTS));
    let empty = SEGMENTS - filled;
    let mut bar = String::with_capacity((SEGMENTS as usize) * 3);
    for _ in 0..filled {
        bar.push('▰');
    }
    for _ in 0..empty {
        bar.push('▱');
    }
    bar
}

/// Paint the in-place "next sync in Ns ▰…" status line. Shared by the
/// ticker's sleep tick AND its reset path so both paint identical-shaped
/// lines (only the elapsed/bar differ). TTY check lives inside
/// `Log::tick_status` so this is safe to call unconditionally on non-TTY.
fn paint_polling_status(renderer: &crate::log::Log, interval_secs: u64, elapsed: u64) {
    let remaining = interval_secs.saturating_sub(elapsed);
    let bar = polling_bar(elapsed, interval_secs);
    let hint = if std::io::stdin().is_terminal() {
        " (press Enter to sync now)"
    } else {
        ""
    };
    renderer.tick_status(
        crate::log::Action::Watch,
        &format!("next sync in {remaining}s {bar}{hint}"),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CycleTrigger {
    /// A local file changed (after debounce).
    FileEvent,
    /// The poll timer fired.
    Poll,
    /// The user pressed Enter on the TTY to sync earlier than the
    /// next scheduled poll. Treated like `Poll` (no debounce). The
    /// stdin reader gates on `sync_running` so a press during an
    /// in-flight cycle is dropped, not queued.
    Manual,
}

/// Everything `event_loop` (and the reconcile before it) needs that a
/// driver other than the CLI's `run_watch` must be able to override:
/// which project directory to treat as cwd, which token to use instead of
/// on-disk secrets, and the same sync flags `rdc sync` exposes on the
/// command line.
///
/// Crate-internal: nothing outside `rdc` constructs one directly today —
/// an embedder goes through a `pub` wrapper (in a later task) that builds
/// this and calls `run_watch_with`. Only `CancelToken` needs to cross that
/// boundary as its own type.
pub(crate) struct WatchConfig<'a> {
    pub env: &'a str,
    pub cwd: Option<&'a Path>,
    pub token: Option<String>,
    pub interactive: bool,
    pub allow_deletes: bool,
    pub no_push: bool,
    pub no_pull: bool,
    pub poll: Option<Duration>,
    pub verbose: bool,
    pub no_bell: bool,
}

/// Refreshes the token for `env` on a 401. `Ok(Some(tok))` is the new token
/// to retry with; `Ok(None)` means the caller refreshed the on-disk secrets
/// in place and `run_cycle` should fall back to re-reading them (the CLI's
/// `inquire`-backed prompt does this). The CLI wires this to
/// `auth::refresh_token_for_401`; an embedder can prompt its own UI.
pub(crate) type TokenRefresher =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<Option<String>>> + Send + Sync>;

/// Installed by the CLI to take ownership of the terminal's stdin. Given
/// the event sender and the `sync_running` flag (a mid-cycle keypress must
/// be dropped, not queued), it spawns the reader. Embedders pass `None`:
/// they answer prompts through a `PromptRoute` and never touch stdin.
pub(crate) type StdinHook =
    Box<dyn FnOnce(tokio::sync::mpsc::Sender<CycleTrigger>, Arc<AtomicBool>) + Send>;

/// Invoked once, immediately after the initial reconcile and before the
/// polling ticker starts. The CLI uses this — not `StdinHook` — to wire
/// Ctrl-C: `StdinHook` only ever runs on a TTY, and gating signal handling
/// on a TTY would leave the non-interactive (piped) case with no way to
/// stop the process. An embedder that manages its own lifecycle (calling
/// `CancelToken::cancel` from its own stop button) passes `None`.
pub(crate) type PostReconcileHook = Box<dyn FnOnce(CancelToken) + Send>;

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
    // Construct the renderer ONCE so freshness clocks persist across cycles.
    let renderer = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
    let cancel = CancelToken::new();

    // Wire ctrl-c → cancel, but only AFTER the initial reconcile — see the
    // `post_reconcile_hook` invocation in `run_watch_with`. Registering
    // tokio's SIGINT handler suppresses the default process-kill, so doing
    // this any earlier would make Ctrl-C inert (no default kill, and
    // nothing reading the cancel flag yet) for the whole of the initial
    // reconcile: the `EnvLock::acquire` wait, a large-org first cycle, and
    // any conflict / remote-delete prompt it blocks on. Non-TTY-gated
    // (unlike the stdin hook below) so the piped/CI case can still be
    // killed.
    let post_reconcile_hook: Option<PostReconcileHook> = Some(Box::new(|cancel: CancelToken| {
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        });
    }));

    // Stdin reader — the SOLE owner of the terminal's stdin while watching
    // (see `cli::stdin_coord`). It reads lines and routes each one:
    //
    //   1. to a waiting interactive prompt (a conflict / remote-delete /
    //      destructive-delete resolver blocked mid-cycle), if one is
    //      registered with the coordinator; else
    //   2. as an Enter-trigger that fires a cycle ahead of the next poll,
    //      when idle; else
    //   3. dropped, when a cycle is running but no prompt is waiting (a
    //      mid-cycle keypress must not queue an extra cycle).
    //
    // Routing through the coordinator is what lets the cycle's prompts and
    // this reader share stdin without fighting over the process-global
    // stdin lock — the resolvers read via the coordinator, never the real
    // stdin, so they can't deadlock against this reader's blocking read.
    //
    // TTY only: in non-interactive contexts (CI piping logs) stdin is EOF
    // or unrelated content, and prompts are non-interactive anyway, so the
    // coordinator is never activated and resolvers read stdin directly.
    //
    // This stays here (built as a `StdinHook`, not inlined in
    // `run_watch_with`) because taking ownership of stdin is CLI-only: an
    // embedder answers prompts through a `PromptRoute` and never touches
    // stdin.
    let stdin_hook: Option<StdinHook> = if std::io::stdin().is_terminal() {
        Some(Box::new(|stdin_tx, stdin_sync_running: Arc<AtomicBool>| {
            // Activate synchronously, before the first event-loop cycle can
            // run a prompt, so the coordinator is live when a prompt registers.
            let coord = crate::cli::stdin_coord::activate();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(tokio::io::stdin()).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    // Hand the line to a waiting prompt first; fall through
                    // only when no prompt wants it.
                    let Err(_) = coord.try_deliver(line) else {
                        continue;
                    };
                    if stdin_sync_running.load(Ordering::Relaxed) {
                        continue;
                    }
                    if stdin_tx.send(CycleTrigger::Manual).await.is_err() {
                        break;
                    }
                }
            });
        }))
    } else {
        None
    };

    let refresher: TokenRefresher =
        Arc::new(|env: String| -> BoxFuture<'static, Result<Option<String>>> {
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
        post_reconcile_hook,
        stdin_hook,
    )
    .await?;

    // Exit the process directly instead of returning `Ok(())` and letting
    // `main` fall into the tokio runtime's `Drop`.
    //
    // The stdin reader task, spawned by the hook built above, is parked in
    // a `tokio::io::stdin()` blocking read, which cannot be cancelled and
    // only returns on EOF. Dropping the multi-threaded runtime blocks
    // until that read completes, so a returned `Ok(())` would hang the
    // process after Ctrl-C until the user also pressed Ctrl-D (EOF) — the
    // exact bug this fixes. We only reach this point via the event loop's
    // shutdown branch (Ctrl-C), so a clean `exit(0)` is the right outcome.
    // The logger flushes on every event, so the "stopped" lines from
    // `run_watch_with` are already on screen. Error paths still propagate
    // through `?` and are handled by `main`'s `exit(1)`, which likewise
    // bypasses the hanging `Drop`.
    std::process::exit(0)
}

/// Runs the reconcile-and-watch loop against `cfg`: the initial reconcile,
/// the polling ticker, the file watcher, and `event_loop`. Everything here
/// is non-terminal-specific and installs no signal handler of its own —
/// this does not take ownership of stdin or exit the process, and Ctrl-C
/// wiring is `post_reconcile_hook`'s job, not this function's, precisely
/// because it must run at a specific point (after the reconcile) rather
/// than unconditionally at entry. Those are `run_watch`'s job (the CLI
/// caller); an embedder drives this directly instead, with its own
/// `CancelToken` and (typically) neither hook.
///
/// Crate-internal for now — see `WatchConfig`'s doc.
pub(crate) async fn run_watch_with(
    cfg: WatchConfig<'_>,
    renderer: Arc<crate::log::Log>,
    cancel: CancelToken,
    refresher: TokenRefresher,
    post_reconcile_hook: Option<PostReconcileHook>,
    stdin_hook: Option<StdinHook>,
) -> Result<()> {
    let cwd = match cfg.cwd {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let paths = crate::paths::Paths::for_env(&cwd, cfg.env);

    // Initial reconcile.
    {
        let _lock =
            crate::cli::sync::lock::EnvLock::acquire(&paths.env_lock(), Duration::from_secs(30))?;
        if !cfg.no_bell {
            crate::cli::stdin_coord::arm_bell();
        }
        crate::cli::sync::run_cycle(
            cfg.env,
            cfg.interactive,
            false,
            cfg.allow_deletes,
            cfg.no_push,
            cfg.no_pull,
            None, // conflict_strategy: `--conflict` is not supported under `--watch`
            Some(renderer.clone()),
            cfg.cwd,
            cfg.token.clone(),
        )
        .await?;
    }

    // Fires exactly once, right after the reconcile above and before the
    // ticker starts. The CLI uses this slot (not entry, not `StdinHook`)
    // to arm Ctrl-C — see `run_watch`'s comment on why timing matters here.
    if let Some(hook) = post_reconcile_hook {
        hook(cancel.clone());
    }

    renderer.event(
        crate::log::Action::Watch,
        &format!("start envs/{}", cfg.env),
    );
    if let Some(d) = cfg.poll {
        renderer.event(
            crate::log::Action::Watch,
            &format!("polling every {}s", d.as_secs()),
        );
    } else {
        renderer.event(crate::log::Action::Watch, "polling disabled");
    }

    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);

    // Shared flag the ticker reads to know when to pause its in-place
    // status drawing. The event_loop flips it true around each cycle.
    let sync_running = Arc::new(AtomicBool::new(false));

    // event_loop fires this after every completed cycle so the ticker's
    // `elapsed` (and the "next sync in Ns" countdown) restarts from zero.
    // Without it, Manual/FileEvent triggers don't touch the ticker's
    // internal clock and the advertised "every Ns" interval drifts to a
    // uniform-random [0, N] gap after any non-Poll cycle.
    let timer_reset = Arc::new(tokio::sync::Notify::new());

    if let Some(interval_duration) = cfg.poll {
        let tx = events_tx.clone();
        let renderer_ticker = renderer.clone();
        let sync_running_ticker = sync_running.clone();
        let reset_ticker = timer_reset.clone();
        let interval_secs = interval_duration.as_secs().max(1);
        tokio::spawn(async move {
            // Tick every second so the status line counts down and the
            // ten-segment bar advances; emit a Poll trigger every
            // `interval_secs` ticks. `reset_ticker.notified()` from
            // event_loop zeroes `elapsed` between sleep ticks so the
            // countdown restarts at `interval_secs` after every cycle.
            let mut elapsed: u64 = 0;
            loop {
                tokio::select! {
                    biased;
                    _ = reset_ticker.notified() => {
                        elapsed = 0;
                        if !sync_running_ticker.load(Ordering::Relaxed) {
                            paint_polling_status(&renderer_ticker, interval_secs, elapsed);
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        elapsed += 1;
                        if elapsed >= interval_secs {
                            elapsed = 0;
                            if tx.send(CycleTrigger::Poll).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        if !sync_running_ticker.load(Ordering::Relaxed) {
                            paint_polling_status(&renderer_ticker, interval_secs, elapsed);
                        }
                    }
                }
            }
        });
    }

    let env_root = paths.env_root();
    let watcher = spawn_file_watcher(cfg.env.to_string(), env_root.clone(), events_tx.clone())?;

    if let Some(hook) = stdin_hook {
        hook(events_tx.clone(), sync_running.clone());
    }

    event_loop(
        &cfg,
        events_rx,
        cancel,
        Some(watcher),
        env_root,
        Some(renderer.clone()),
        sync_running.clone(),
        timer_reset.clone(),
        &refresher,
    )
    .await?;
    renderer.finish_status();
    // Owner-of-renderer finalization: run_cycle skips the Done event when a
    // persistent renderer was supplied (otherwise the grid would freeze
    // after the first cycle), so the watch loop emits it here on exit.
    renderer.event(crate::log::Action::Done, "stopped watch");
    renderer.event(crate::log::Action::Watch, "stopped");
    Ok(())
}

/// The testable inner loop: drain events, run cycles, exit on shutdown.
/// Tests call this directly with synthetic channels.
///
/// `timer_reset` is notified once per completed cycle (every exit path of
/// the cycle handler) so the polling ticker — which counts independently
/// of when cycles fire — restarts its `interval_secs` countdown from the
/// moment the cycle ended, not from some prior baseline.
pub(crate) async fn event_loop(
    cfg: &WatchConfig<'_>,
    mut events: tokio::sync::mpsc::Receiver<CycleTrigger>,
    cancel: CancelToken,
    mut watcher: Option<notify::RecommendedWatcher>,
    env_root: std::path::PathBuf,
    renderer: Option<Arc<crate::log::Log>>,
    sync_running: Arc<AtomicBool>,
    timer_reset: Arc<tokio::sync::Notify>,
    refresher: &TokenRefresher,
) -> Result<()> {
    use notify::{RecursiveMode, Watcher};

    let cwd = match cfg.cwd {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let paths = crate::paths::Paths::for_env(&cwd, cfg.env);

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            evt = events.recv() => {
                let Some(trigger) = evt else { break };
                // Debounce only file events. Poll events run immediately.
                if matches!(trigger, CycleTrigger::FileEvent) {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                // Coalesce any pending events that arrived during the debounce window
                // (or during a previous cycle execution).
                while events.try_recv().is_ok() {}

                // Pause the watcher around our own writes to avoid feedback loops.
                if let Some(w) = watcher.as_mut() {
                    let _ = w.unwatch(&env_root);
                }

                // Restart the polling countdown after the cycle ends, on
                // every exit path (Ok / non-fatal Err continue / `?`
                // return). Declared FIRST so it drops LAST — by the time
                // `notify_one` fires, the CycleGuard below has already
                // cleared `sync_running`, so the ticker's reset branch
                // sees the cycle as complete and immediately repaints a
                // fresh "next sync in {interval_secs}s ▱…" line.
                struct ResetOnDrop<'a>(&'a tokio::sync::Notify);
                impl Drop for ResetOnDrop<'_> {
                    fn drop(&mut self) { self.0.notify_one(); }
                }
                let _reset_guard = ResetOnDrop(&timer_reset);

                let _cycle_started = std::time::Instant::now();
                let _lock = crate::cli::sync::lock::EnvLock::acquire(
                    &paths.env_lock(),
                    std::time::Duration::from_secs(30),
                )?;
                // Suspend the polling-status ticker for the duration of
                // the cycle so its in-place updates don't tear with the
                // cycle's regular event lines. Reset on every exit path
                // below via the RAII guard.
                struct CycleGuard<'a>(&'a AtomicBool);
                impl Drop for CycleGuard<'_> {
                    fn drop(&mut self) { self.0.store(false, Ordering::Relaxed); }
                }
                sync_running.store(true, Ordering::Relaxed);
                let _cycle_guard = CycleGuard(&sync_running);
                // Re-arm the attention bell once per cycle so the first
                // blocking prompt (conflict / delete / drift / 401) rings.
                if !cfg.no_bell {
                    crate::cli::stdin_coord::arm_bell();
                }
                let _outcome = match crate::cli::sync::run_cycle(
                    cfg.env, cfg.interactive, false, cfg.allow_deletes, cfg.no_push, cfg.no_pull,
                    None, renderer.clone(), cfg.cwd, cfg.token.clone(),
                ).await {
                    Ok(o) => o,
                    Err(e) if crate::api::anyhow_has_status(&e, 401) => {
                        // Prompt for a new token inline; retry once. Surface
                        // via the renderer's banner so the grid stays visible.
                        //
                        // The attention bell deliberately does NOT ring for this
                        // prompt: `refresh_token_for_401` uses inquire (raw fd-0
                        // reads) which races the always-running stdin reader
                        // under watch, so the token can be mangled and the prompt
                        // is likely unanswerable here. Pre-existing; tracked in
                        // issue #2. Re-enable the ring once that is fixed.
                        if let Some(r) = renderer.as_ref() {
                            r.event(crate::log::Action::Auth, "token expired — refreshing");
                        } else {
                            eprintln!("auth: token expired");
                        }
                        let fresh = refresher(cfg.env.to_string()).await?;
                        let token = fresh.or_else(|| cfg.token.clone());
                        crate::cli::sync::run_cycle(
                            cfg.env, cfg.interactive, false, cfg.allow_deletes, cfg.no_push, cfg.no_pull,
                            None, renderer.clone(), cfg.cwd, token,
                        ).await?
                    }
                    Err(e) if is_transient_network_error(&e) => {
                        if let Some(r) = renderer.as_ref() {
                            r.event(crate::log::Action::Watch, &format!("cycle failed (transient): {e:#}"));
                        } else {
                            eprintln!("watch: cycle failed (transient): {e:#}");
                        }
                        // Resume watcher and continue to next iteration.
                        if let Some(w) = watcher.as_mut() {
                            let _ = w.watch(&env_root, RecursiveMode::Recursive);
                        }
                        while events.try_recv().is_ok() {}
                        continue;
                    }
                    Err(e) if is_local_parse_error(&e) => {
                        if let Some(r) = renderer.as_ref() {
                            r.event(crate::log::Action::Watch, &format!("cycle failed (local file error): {e:#}"));
                        } else {
                            eprintln!("watch: cycle failed (local file error): {e:#}");
                        }
                        if let Some(w) = watcher.as_mut() {
                            let _ = w.watch(&env_root, RecursiveMode::Recursive);
                        }
                        while events.try_recv().is_ok() {}
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                drop(_lock);

                // Resume watching. Drop any events that arrived during the pause —
                // those events are our own writes.
                if let Some(w) = watcher.as_mut() {
                    let _ = w.watch(&env_root, RecursiveMode::Recursive);
                }
                while events.try_recv().is_ok() {}

                // The grid renderer (when active) IS the cycle summary:
                // counts repaint as the cycle progresses, freshness clocks
                // bump on each ingest. The log renderer's per-cycle output
                // also already shows the summary via `progress.finish_ok`
                // inside `run_cycle`. `verbose` is retained on the signature
                // for backward compatibility but has no effect today.
                let _ = cfg.verbose;
            }
        }
    }
    Ok(())
}

/// Heuristic: does this error look like a transient network failure?
/// Refine if false positives surface in integration tests.
fn is_transient_network_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string();
        s.contains("timed out")
            || s.contains("connection refused")
            || s.contains("connection reset")
            || s.contains("5xx")
            || s.contains("Connection")
    })
}

/// Heuristic: does this error look like a local-file parse failure?
fn is_local_parse_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string();
        s.contains("invalid JSON") || s.contains("serde_json") || s.contains("expected value")
    })
}

fn spawn_file_watcher(
    env: String,
    env_root: std::path::PathBuf,
    tx: tokio::sync::mpsc::Sender<CycleTrigger>,
) -> Result<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};

    let event_handler = move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else {
            return;
        };
        for path in &event.paths {
            if path_should_be_ignored(path, &env) {
                continue;
            }
            // Non-blocking send: if the channel is full, the cycle worker is
            // behind — dropping a triggering event is fine since events
            // coalesce anyway.
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
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // `_index.md` is regenerated by every cycle (`cli::index::generate`).
    // Treating its write as a trigger feeds the cycle back into itself —
    // notably bad on fsevents/macOS where the rename event can land after
    // the post-cycle drain due to inherent stream latency.
    if name == "_index.md" {
        return true;
    }
    // Ignore shadow artifacts.
    crate::paths::is_shadow_artifact(name, env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex as StdMutex, MutexGuard, OnceLock};
    use tokio::sync::mpsc;

    /// Guard returned by [`cwd_lock`]: holds the global mutex AND restores
    /// the working directory captured at lock time when dropped —
    /// including on panic.
    ///
    /// Duplicated from `tests/cli_migrate.rs`'s helper of the same name
    /// rather than shared, since that file is a separate integration-test
    /// binary and this is a `--lib` unit test module; there is no existing
    /// `#[cfg(test)]`-only location both already depend on, and this is
    /// the only file under `src/` whose tests touch the process cwd (see
    /// `grep -rn set_current_dir src/`), so a shared module would have
    /// exactly one caller on this side anyway.
    ///
    /// Without the restore, a test that panics inside its
    /// `set_current_dir` window leaves the process cwd pointing into its
    /// (now deleted) tempdir and every later test in this binary that
    /// reads `current_dir()` fails with `NotFound` — this is exactly the
    /// failure `event_loop_uses_the_injected_cwd_not_the_process_cwd` and
    /// `event_loop_exits_cleanly_on_shutdown` could inflict on each other
    /// (and any future cwd-touching test in this module) when `cargo test`
    /// runs them on parallel threads, since both mutate the one
    /// process-wide cwd with no serialization.
    struct CwdLock {
        _lock: MutexGuard<'static, ()>,
        prev: Option<std::path::PathBuf>,
    }

    impl Drop for CwdLock {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                let _ = std::env::set_current_dir(prev);
            }
        }
    }

    fn cwd_lock() -> CwdLock {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        let lock = LOCK
            .get_or_init(|| StdMutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        CwdLock {
            _lock: lock,
            prev: std::env::current_dir().ok(),
        }
    }

    #[test]
    fn polling_bar_tenths() {
        // Within a 60s interval, each segment is 6 seconds.
        assert_eq!(polling_bar(0, 60), "▱▱▱▱▱▱▱▱▱▱");
        assert_eq!(polling_bar(5, 60), "▱▱▱▱▱▱▱▱▱▱");
        assert_eq!(polling_bar(6, 60), "▰▱▱▱▱▱▱▱▱▱");
        assert_eq!(polling_bar(24, 60), "▰▰▰▰▱▱▱▱▱▱");
        assert_eq!(polling_bar(30, 60), "▰▰▰▰▰▱▱▱▱▱");
        assert_eq!(polling_bar(54, 60), "▰▰▰▰▰▰▰▰▰▱");
        assert_eq!(polling_bar(59, 60), "▰▰▰▰▰▰▰▰▰▱");
    }

    #[test]
    fn polling_bar_handles_overflow_and_zero_interval() {
        // elapsed >= total: full bar.
        assert_eq!(polling_bar(60, 60), "▰▰▰▰▰▰▰▰▰▰");
        assert_eq!(polling_bar(120, 60), "▰▰▰▰▰▰▰▰▰▰");
        // total = 0 (defensive): return the full bar instead of dividing.
        assert_eq!(polling_bar(0, 0), "▰▰▰▰▰▰▰▰▰▰");
    }

    #[tokio::test]
    async fn event_loop_exits_cleanly_on_shutdown() {
        // Holds the process-wide cwd mutex for the rest of this test and
        // restores the pre-test cwd on drop (including on panic) — see
        // `CwdLock`'s doc. Without it this test and
        // `event_loop_uses_the_injected_cwd_not_the_process_cwd` race on
        // `std::env::set_current_dir` under `cargo test`'s parallel
        // threads.
        let _cwd_guard = cwd_lock();

        let (_tx, rx) = mpsc::channel::<CycleTrigger>(8);

        // event_loop expects a project context — without one, the lock acquire
        // would fail on a non-existent .rdc/state/ dir. For this minimal test,
        // we shut down BEFORE any event arrives, so run_cycle is never called.

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".rdc/state")).unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let cancel = CancelToken::new();
        cancel.cancel(); // shutdown before any event

        let cfg = WatchConfig {
            env: "test",
            cwd: None, // covers the CLI path: event_loop falls back to process cwd
            token: None,
            interactive: false,
            allow_deletes: false,
            no_push: false,
            no_pull: false,
            poll: None,
            verbose: false,
            no_bell: false,
        };
        let refresher: TokenRefresher =
            Arc::new(|_: String| -> BoxFuture<'static, Result<Option<String>>> {
                Box::pin(async { Ok(None) })
            });
        let result = event_loop(
            &cfg,
            rx,
            cancel,
            None,
            std::path::PathBuf::new(),
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(tokio::sync::Notify::new()),
            &refresher,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
    }

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
        // The process CWD points at an empty, unrelated tempdir; `cfg.cwd`
        // points at a DIFFERENT tempdir standing in for the real project
        // root. A Poll trigger (not pre-cancelled) drives exactly one
        // cycle. `run_cycle` is bound to fail — there is no rdc.toml in
        // either tempdir — but `EnvLock::acquire` writes the lock file
        // before that failure, so its location is proof of which root the
        // loop actually used. If `cfg.cwd` were ignored, the lock would
        // land under the process CWD instead (see the falsification note
        // in the task 7 report: flipping `cwd` to `None` here does fail
        // this assertion, and only this one).
        // See `event_loop_exits_cleanly_on_shutdown`'s comment on `CwdLock`:
        // this test also mutates the process-wide cwd.
        let _cwd_guard = cwd_lock();

        let process_cwd = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_current_dir(process_cwd.path()).unwrap();

        let (tx, rx) = tokio::sync::mpsc::channel::<CycleTrigger>(8);
        tx.send(CycleTrigger::Poll).await.unwrap();
        let cancel = CancelToken::new(); // NOT pre-cancelled: the cycle must actually run

        let cfg = WatchConfig {
            env: "test",
            cwd: Some(project.path()),
            token: Some("tok".into()),
            interactive: false,
            allow_deletes: false,
            no_push: false,
            no_pull: false,
            poll: None,
            verbose: false,
            no_bell: true,
        };
        let refresher: TokenRefresher =
            Arc::new(|_: String| -> BoxFuture<'static, Result<Option<String>>> {
                Box::pin(async { Ok(None) })
            });
        let result = event_loop(
            &cfg,
            rx,
            cancel,
            None,
            project.path().join("envs/test"),
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(tokio::sync::Notify::new()),
            &refresher,
        )
        .await;

        // The cycle fails (no rdc.toml anywhere) — that failure is not
        // what this test is about.
        assert!(result.is_err(), "{result:?}");
        assert!(
            project.path().join(".rdc/state/test.lock").exists(),
            "EnvLock::acquire should have created the lock file under the injected cwd"
        );
        assert!(
            !process_cwd.path().join(".rdc").exists(),
            "the process CWD must never be touched when cfg.cwd is set"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn poll_interval_produces_one_event_per_tick() {
        use std::time::Duration;
        use tokio::sync::mpsc;

        let (tx, mut rx) = mpsc::channel::<CycleTrigger>(8);
        let interval = Duration::from_secs(60);
        let _h = tokio::spawn(async move {
            let mut t = tokio::time::interval(interval);
            t.tick().await; // skip first
            loop {
                t.tick().await;
                if tx.send(CycleTrigger::Poll).await.is_err() {
                    break;
                }
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

    #[tokio::test(start_paused = true)]
    async fn timer_reset_via_notify_postpones_next_poll() {
        // Spec test (mirrors the ticker loop in `run_watch`). Demonstrates
        // that a `notify_one()` on the reset channel zeroes `elapsed` so
        // the next Poll fires `interval_secs` after the reset — not
        // `interval_secs - elapsed_at_reset`.
        use std::time::Duration;
        use tokio::sync::{Notify, mpsc};
        let (tx, mut rx) = mpsc::channel::<CycleTrigger>(8);
        let reset = Arc::new(Notify::new());
        let interval_secs: u64 = 5;

        let reset_ticker = reset.clone();
        let _h = tokio::spawn(async move {
            let mut elapsed: u64 = 0;
            loop {
                tokio::select! {
                    biased;
                    _ = reset_ticker.notified() => { elapsed = 0; }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        elapsed += 1;
                        if elapsed >= interval_secs {
                            elapsed = 0;
                            if tx.send(CycleTrigger::Poll).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });

        // Tick to elapsed=3 (interval=5, no Poll yet).
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert!(rx.try_recv().is_err(), "Poll fired early");

        // Reset. elapsed -> 0. Without the reset, 2s more would fire Poll.
        reset.notify_one();
        tokio::task::yield_now().await;

        // Tick 4 more seconds: total 7s since start (would have fired
        // twice without the reset) but only 4s since reset. No Poll yet.
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert!(
            rx.try_recv().is_err(),
            "Poll fired before {interval_secs}s elapsed after reset"
        );

        // One more tick → 5s since reset → Poll.
        tokio::time::advance(Duration::from_secs(1)).await;
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt, CycleTrigger::Poll);
    }

    async fn drain_after_debounce<T>(rx: &mut tokio::sync::mpsc::Receiver<T>) -> usize {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let mut drained = 0;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        drained
    }

    #[tokio::test(start_paused = true)]
    async fn manual_trigger_skipped_when_sync_running_else_forwarded() {
        // Models the stdin reader's behavior: drop lines that arrive
        // while `sync_running` is true, forward otherwise. The actual
        // reader is shaped exactly the same loop.
        let (tx, mut rx) = mpsc::channel::<CycleTrigger>(8);
        let sync_running = Arc::new(AtomicBool::new(false));

        let lines = vec![()].into_iter(); // one "Enter" press
        for _ in lines {
            if !sync_running.load(Ordering::Relaxed) {
                tx.send(CycleTrigger::Manual).await.unwrap();
            }
        }
        assert_eq!(rx.recv().await, Some(CycleTrigger::Manual));

        // Now simulate sync_running and a press during the cycle.
        sync_running.store(true, Ordering::Relaxed);
        for _ in 0..3 {
            if !sync_running.load(Ordering::Relaxed) {
                tx.send(CycleTrigger::Manual).await.unwrap();
            }
        }
        // No event should have been queued.
        assert!(rx.try_recv().is_err(), "press during sync should drop");

        // Cycle ends; subsequent press goes through.
        sync_running.store(false, Ordering::Relaxed);
        if !sync_running.load(Ordering::Relaxed) {
            tx.send(CycleTrigger::Manual).await.unwrap();
        }
        assert_eq!(rx.recv().await, Some(CycleTrigger::Manual));
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

    #[test]
    fn path_should_be_ignored_rejects_rdc_subtree() {
        assert!(path_should_be_ignored(
            std::path::Path::new("/proj/.rdc/state/test.lock.json"),
            "test"
        ));
    }

    #[test]
    fn path_should_be_ignored_rejects_generated_index_md() {
        // `_index.md` is rewritten every cycle; treating its write as a
        // trigger would loop the cycle into itself.
        assert!(path_should_be_ignored(
            std::path::Path::new("/proj/envs/test/_index.md"),
            "test"
        ));
        // Other envs' generated index is also ignored (no env-suffix
        // filtering needed — `_index.md` is unique per env directory).
        assert!(path_should_be_ignored(
            std::path::Path::new("/proj/envs/other/_index.md"),
            "test"
        ));
        // But a user-authored file that happens to start with `_` is fine.
        assert!(!path_should_be_ignored(
            std::path::Path::new("/proj/envs/test/_notes.md"),
            "test"
        ));
    }

    #[test]
    fn path_should_be_ignored_rejects_shadow_files() {
        assert!(path_should_be_ignored(
            std::path::Path::new("/proj/envs/test/labels/a.json.test"),
            "test"
        ));
        assert!(path_should_be_ignored(
            std::path::Path::new("/proj/envs/test/labels/a.json.test-deleted"),
            "test"
        ));
    }

    #[test]
    fn path_should_be_ignored_accepts_normal_files() {
        assert!(!path_should_be_ignored(
            std::path::Path::new("/proj/envs/test/labels/a.json"),
            "test"
        ));
        assert!(!path_should_be_ignored(
            std::path::Path::new("/proj/envs/test/overlay.toml"),
            "test"
        ));
    }

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

    #[test]
    fn unknown_error_recognizes_neither() {
        let e = anyhow::anyhow!("something totally else");
        assert!(!is_transient_network_error(&e));
        assert!(!is_local_parse_error(&e));
    }
}
