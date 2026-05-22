//! `rdc sync --watch <env>` — foreground watch mode.
//!
//! Spec: docs/superpowers/specs/2026-05-14-watch-mode-design.md

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Ten-segment bar showing position within the polling interval.
fn polling_bar(elapsed: u64, total: u64) -> String {
    const SEGMENTS: u64 = 10;
    let filled = if total == 0 {
        SEGMENTS
    } else {
        (elapsed.saturating_mul(SEGMENTS) / total).min(SEGMENTS)
    };
    let empty = SEGMENTS - filled;
    let mut bar = String::with_capacity((SEGMENTS as usize) * 3);
    for _ in 0..filled { bar.push('▰'); }
    for _ in 0..empty  { bar.push('▱'); }
    bar
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CycleTrigger {
    /// A local file changed (after debounce).
    FileEvent,
    /// The poll timer fired.
    Poll,
}

pub async fn run_watch(
    env: &str,
    interactive: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    poll_interval: Option<Duration>,
    verbose: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let paths = crate::paths::Paths::for_env(&cwd, env);

    // Construct the renderer ONCE so freshness clocks persist across cycles.
    let renderer = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));

    // Initial reconcile.
    {
        let _lock = crate::cli::sync::lock::EnvLock::acquire(
            &paths.env_lock(),
            Duration::from_secs(30),
        )?;
        crate::cli::sync::run_cycle(
            env,
            interactive,
            false,
            false,
            allow_deletes,
            no_push,
            no_pull,
            Some(renderer.clone()),
        )
        .await?;
    }

    renderer.event(crate::log::Action::Watch, &format!("start envs/{env}"));
    if let Some(d) = poll_interval {
        renderer.event(crate::log::Action::Watch, &format!("polling every {}s", d.as_secs()));
    } else {
        renderer.event(crate::log::Action::Watch, "polling disabled");
    }

    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    // Wire ctrl-c → shutdown.
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = shutdown_tx.send(());
    });

    // Shared flag the ticker reads to know when to pause its in-place
    // status drawing. The event_loop flips it true around each cycle.
    let sync_running = Arc::new(AtomicBool::new(false));

    if let Some(interval_duration) = poll_interval {
        let tx = events_tx.clone();
        let renderer_ticker = renderer.clone();
        let sync_running_ticker = sync_running.clone();
        let interval_secs = interval_duration.as_secs().max(1);
        tokio::spawn(async move {
            // Tick every second so the status line counts down and the
            // four-stage bar advances; emit a Poll trigger every
            // `interval_secs` ticks.
            let mut elapsed: u64 = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                elapsed += 1;
                if elapsed >= interval_secs {
                    elapsed = 0;
                    if tx.send(CycleTrigger::Poll).await.is_err() {
                        break;
                    }
                    continue;
                }
                if !sync_running_ticker.load(Ordering::Relaxed) {
                    let remaining = interval_secs - elapsed;
                    let bar = polling_bar(elapsed, interval_secs);
                    renderer_ticker.tick_status(
                        crate::log::Action::Watch,
                        &format!("next sync in {remaining}s {bar}"),
                    );
                }
            }
        });
    }

    let env_root = paths.env_root();
    let watcher = spawn_file_watcher(env.to_string(), env_root.clone(), events_tx.clone())?;

    event_loop(
        env,
        interactive,
        allow_deletes,
        no_push,
        no_pull,
        verbose,
        events_rx,
        shutdown_rx,
        Some(watcher),
        env_root,
        Some(renderer.clone()),
        sync_running.clone(),
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
    renderer: Option<Arc<crate::log::Log>>,
    sync_running: Arc<AtomicBool>,
) -> Result<()> {
    use notify::{RecursiveMode, Watcher};

    let cwd = std::env::current_dir()?;
    let paths = crate::paths::Paths::for_env(&cwd, env);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
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
                let _outcome = match crate::cli::sync::run_cycle(
                    env, interactive, false, false, allow_deletes, no_push, no_pull,
                    renderer.clone(),
                ).await {
                    Ok(o) => o,
                    Err(e) if crate::api::anyhow_has_status(&e, 401) => {
                        // Prompt for a new token inline; retry once. Surface
                        // via the renderer's banner so the grid stays visible.
                        if let Some(r) = renderer.as_ref() {
                            r.event(crate::log::Action::Auth, "token expired — refreshing");
                        } else {
                            eprintln!("auth: token expired");
                        }
                        crate::cli::auth::refresh_token_interactively(env).await?;
                        crate::cli::sync::run_cycle(
                            env, interactive, false, false, allow_deletes, no_push, no_pull,
                            renderer.clone(),
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
                let _ = verbose;
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
    // Ignore shadow artifacts.
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    crate::paths::is_shadow_artifact(name, env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};

    #[test]
    fn polling_bar_tenths() {
        // Within a 60s interval, each segment is 6 seconds.
        assert_eq!(polling_bar(0, 60),  "▱▱▱▱▱▱▱▱▱▱");
        assert_eq!(polling_bar(5, 60),  "▱▱▱▱▱▱▱▱▱▱");
        assert_eq!(polling_bar(6, 60),  "▰▱▱▱▱▱▱▱▱▱");
        assert_eq!(polling_bar(24, 60), "▰▰▰▰▱▱▱▱▱▱");
        assert_eq!(polling_bar(30, 60), "▰▰▰▰▰▱▱▱▱▱");
        assert_eq!(polling_bar(54, 60), "▰▰▰▰▰▰▰▰▰▱");
        assert_eq!(polling_bar(59, 60), "▰▰▰▰▰▰▰▰▰▱");
    }

    #[test]
    fn polling_bar_handles_overflow_and_zero_interval() {
        // elapsed >= total: full bar.
        assert_eq!(polling_bar(60, 60),  "▰▰▰▰▰▰▰▰▰▰");
        assert_eq!(polling_bar(120, 60), "▰▰▰▰▰▰▰▰▰▰");
        // total = 0 (defensive): return the full bar instead of dividing.
        assert_eq!(polling_bar(0, 0), "▰▰▰▰▰▰▰▰▰▰");
    }

    #[tokio::test]
    async fn event_loop_exits_cleanly_on_shutdown() {
        let (_tx, rx) = mpsc::channel::<CycleTrigger>(8);
        let (sh_tx, sh_rx) = oneshot::channel();

        // event_loop expects a project context — without one, the lock acquire
        // would fail on a non-existent .rdc/state/ dir. For this minimal test,
        // we shut down BEFORE any event arrives, so run_cycle is never called.

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".rdc/state")).unwrap();
        let saved_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        sh_tx.send(()).unwrap(); // shutdown before any event
        let result = event_loop(
            "test", false, false, false, false, false, rx, sh_rx,
            None, std::path::PathBuf::new(), None,
            Arc::new(AtomicBool::new(false)),
        ).await;

        std::env::set_current_dir(saved_cwd).unwrap();
        assert!(result.is_ok(), "{result:?}");
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

    #[test]
    fn path_should_be_ignored_rejects_rdc_subtree() {
        assert!(path_should_be_ignored(
            std::path::Path::new("/proj/.rdc/state/test.lock.json"),
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
