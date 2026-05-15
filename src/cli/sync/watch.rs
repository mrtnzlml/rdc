//! `rdc sync --watch <env>` — foreground watch mode.
//!
//! Spec: docs/superpowers/specs/2026-05-14-watch-mode-design.md

use anyhow::Result;
use std::time::Duration;

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
            true,
        )
        .await?;
    }

    eprintln!("watching envs/{env}/ ...");
    if let Some(d) = poll_interval {
        eprintln!("polling {env} every {}s ...", d.as_secs());
    } else {
        eprintln!("polling disabled");
    }

    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    // Wire ctrl-c → shutdown.
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = shutdown_tx.send(());
    });

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

    let env_root = paths.env_root();
    let _watcher = spawn_file_watcher(env.to_string(), env_root, events_tx.clone())?;

    event_loop(
        env,
        interactive,
        allow_deletes,
        no_push,
        no_pull,
        verbose,
        events_rx,
        shutdown_rx,
    )
    .await?;
    eprintln!("\nstopping watch.");
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
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let paths = crate::paths::Paths::for_env(&cwd, env);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            evt = events.recv() => {
                let Some(_trigger) = evt else { break };
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
                print_cycle_summary(&outcome, elapsed, verbose);
            }
        }
    }
    Ok(())
}

fn print_cycle_summary(
    outcome: &crate::cli::sync::CycleOutcome,
    elapsed: std::time::Duration,
    verbose: bool,
) {
    let total = outcome.items_pushed
        + outcome.items_pulled
        + outcome.conflicts
        + outcome.remote_deletes_resolved;
    if total == 0 && !verbose {
        return; // quiet by default
    }
    let now = now_hhmmss();
    let dir = if outcome.items_pulled > 0 && outcome.items_pushed == 0 {
        "\u{2190}" // ←
    } else if outcome.items_pushed > 0 && outcome.items_pulled == 0 {
        "\u{2192}" // →
    } else {
        "\u{2194}" // ↔
    };
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

/// UTC HH:MM:SS — small standalone formatter so we don't add a `chrono` dep.
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
        let result = event_loop("test", false, false, false, false, false, rx, sh_rx).await;

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
}
