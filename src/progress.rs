//! Overall progress UX during pull/push.
//!
//! - TTY: a single `indicatif` bar with a phase label that swaps as
//!   drivers run. Warnings print above the bar via `println` (cleanly
//!   redrawn). Cheap `Arc` clone for use inside concurrent closures.
//! - Non-TTY (CI, piped): plain `→ phase: …` then `✓ phase: N items, Xs`
//!   lines per phase. The same shape integration tests have asserted on.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Optional reference to the run-wide progress bar. `None` when no bar
/// is active (e.g. `rdc auth`, `rdc diff`). Cheap to clone; just bumps
/// an `Arc` refcount.
pub type ProgressHandle = Option<Arc<OverallProgress>>;

/// Cheaply cloneable handle to the run-wide progress UX. Wrap your only
/// instance in `Arc<OverallProgress>` and clone it everywhere — concurrent
/// closures included.
pub struct OverallProgress {
    inner: Mutex<Inner>,
}

enum Inner {
    Bar {
        bar: indicatif::ProgressBar,
        current_phase: Option<String>,
        phase_started: Option<Instant>,
        phase_count: AtomicU64,
        orphans: AtomicUsize,
        finished: bool,
    },
    Log {
        current_phase: Option<String>,
        phase_started: Option<Instant>,
        phase_count: AtomicU64,
        total_count: AtomicU64,
        orphans: AtomicUsize,
        finished: bool,
    },
}

impl OverallProgress {
    /// Create the run-wide progress. `title` is the prefix line, e.g.
    /// `"pull envs/dev"` or `"push envs/dev"`.
    pub fn start(title: impl Into<String>) -> Arc<Self> {
        let title: String = title.into();
        let inner = if std::io::stderr().is_terminal() {
            let bar = indicatif::ProgressBar::new(0);
            bar.set_prefix(title);
            bar.set_style(
                indicatif::ProgressStyle::with_template(
                    "{spinner} {prefix}  [{wide_bar}] {pos}/{len}  ETA {eta}\n  ↳ {msg}",
                )
                .unwrap(),
            );
            bar.set_message("discovering items…");
            bar.enable_steady_tick(std::time::Duration::from_millis(100));
            Inner::Bar {
                bar,
                current_phase: None,
                phase_started: None,
                phase_count: AtomicU64::new(0),
                orphans: AtomicUsize::new(0),
                finished: false,
            }
        } else {
            eprintln!("→ {title}: discovering items…");
            Inner::Log {
                current_phase: None,
                phase_started: None,
                phase_count: AtomicU64::new(0),
                total_count: AtomicU64::new(0),
                orphans: AtomicUsize::new(0),
                finished: false,
            }
        };
        Arc::new(Self { inner: Mutex::new(inner) })
    }

    /// Increase the bar's total denominator by `n`. Call this each time a
    /// phase reports its newly-listed item count, before any of those
    /// items get processed.
    pub fn inc_total(&self, n: u64) {
        let inner = self.inner.lock().unwrap();
        if let Inner::Bar { bar, .. } = &*inner {
            bar.inc_length(n);
        }
        // Log mode: nothing to do; total is implicit.
    }

    /// Switch to a new phase. Emits a per-phase done-line (in log mode) or
    /// updates the bar's sub-label (in TTY mode) for the previous phase
    /// before transitioning. The phase counter resets.
    ///
    /// Returns the (count, orphans, duration) of the phase that just
    /// ended, or None if this is the first phase.
    pub fn start_phase(&self, name: impl Into<String>) -> Option<(u64, usize, std::time::Duration)> {
        let mut inner = self.inner.lock().unwrap();
        let (prev, ended_stats) = end_current_phase(&mut inner);
        let name: String = name.into();
        match &mut *inner {
            Inner::Bar { current_phase, phase_started, phase_count, .. } => {
                *current_phase = Some(name.clone());
                *phase_started = Some(Instant::now());
                phase_count.store(0, Ordering::Relaxed);
            }
            Inner::Log { current_phase, phase_started, phase_count, .. } => {
                *current_phase = Some(name.clone());
                *phase_started = Some(Instant::now());
                phase_count.store(0, Ordering::Relaxed);
                eprintln!("→ {name}: starting");
            }
        }
        let _ = prev;
        ended_stats
    }

    /// Advance the bar by one and update the sub-label to `item`. Safe to
    /// call from concurrent closures (Arc + Mutex serializes; indicatif's
    /// inc/set_message are thread-safe internally).
    pub fn tick(&self, item: impl AsRef<str>) {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { bar, current_phase, phase_count, .. } => {
                phase_count.fetch_add(1, Ordering::Relaxed);
                bar.inc(1);
                let phase = current_phase.as_deref().unwrap_or("");
                let item = item.as_ref();
                bar.set_message(format!("{phase}: {item}"));
            }
            Inner::Log { phase_count, total_count, .. } => {
                phase_count.fetch_add(1, Ordering::Relaxed);
                total_count.fetch_add(1, Ordering::Relaxed);
                let _ = item;
            }
        }
    }

    /// Increment the orphan-skipped counter (does not advance the bar).
    pub fn skipped_orphan(&self) {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { orphans, .. } => { orphans.fetch_add(1, Ordering::Relaxed); }
            Inner::Log { orphans, .. } => { orphans.fetch_add(1, Ordering::Relaxed); }
        }
    }

    /// Print a line above the bar (TTY) or to stderr (log). Use for
    /// retry warnings, conflict notes, drift refusals, etc. Replaces the
    /// older `suspend(|| eprintln!(...))` idiom.
    pub fn println(&self, msg: impl AsRef<str>) {
        let inner = self.inner.lock().unwrap();
        let msg = msg.as_ref();
        match &*inner {
            Inner::Bar { bar, .. } => bar.println(msg),
            Inner::Log { .. } => eprintln!("{msg}"),
        }
    }

    /// Read the current orphan count. Useful for the final summary line.
    pub fn orphans(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { orphans, .. } => orphans.load(Ordering::Relaxed),
            Inner::Log { orphans, .. } => orphans.load(Ordering::Relaxed),
        }
    }

    /// Read the bar's current position (total items processed across all
    /// phases). Useful for the final summary line.
    pub fn total_processed(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { bar, .. } => bar.position(),
            Inner::Log { total_count, .. } => total_count.load(Ordering::Relaxed),
        }
    }

    /// Finish the run. Closes the current phase, then the bar/log. Caller
    /// holds the only `Arc<OverallProgress>` clone; we cannot consume
    /// `self` (Arc forbids it), so this leaves `inner.finished = true`
    /// and Drop is a no-op.
    pub fn finish(&self) {
        let mut inner = self.inner.lock().unwrap();
        let _ended = end_current_phase(&mut inner);
        match &mut *inner {
            Inner::Bar { bar, finished, .. } => {
                *finished = true;
                bar.finish_and_clear();
            }
            Inner::Log { finished, .. } => {
                *finished = true;
            }
        }
    }
}

impl Drop for OverallProgress {
    fn drop(&mut self) {
        let inner = self.inner.try_lock();
        if let Ok(mut inner) = inner {
            let already_finished = matches!(
                &*inner,
                Inner::Bar { finished: true, .. } | Inner::Log { finished: true, .. }
            );
            if !already_finished {
                if let Inner::Bar { bar, .. } = &*inner {
                    bar.finish_and_clear();
                }
                let _ = end_current_phase(&mut inner);
            }
        }
    }
}

fn end_current_phase(inner: &mut Inner) -> (Option<String>, Option<(u64, usize, std::time::Duration)>) {
    match inner {
        Inner::Bar { current_phase, phase_started, phase_count, orphans, .. } => {
            let Some(name) = current_phase.take() else { return (None, None); };
            let started = phase_started.take().unwrap_or_else(Instant::now);
            let dur = started.elapsed();
            let count = phase_count.swap(0, Ordering::Relaxed);
            // Don't reset the orphan counter (orphans accumulate across phases).
            let orphans_n = orphans.load(Ordering::Relaxed);
            (Some(name), Some((count, orphans_n, dur)))
        }
        Inner::Log { current_phase, phase_started, phase_count, orphans, .. } => {
            let Some(name) = current_phase.take() else { return (None, None); };
            let started = phase_started.take().unwrap_or_else(Instant::now);
            let dur = started.elapsed();
            let count = phase_count.swap(0, Ordering::Relaxed);
            let orphans_n = orphans.load(Ordering::Relaxed);
            // Per-phase done line in log mode. (Per-phase orphans aren't
            // tracked separately yet — the global counter is fine for
            // the final summary; per-phase only matters in TTY's sub-label.)
            eprintln!("✓ {name}: {count} items, {:.1}s", dur.as_secs_f32());
            (Some(name), Some((count, orphans_n, dur)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_clone_smoke() {
        let p = OverallProgress::start("pull envs/test");
        let clone = Arc::clone(&p);
        clone.tick("foo");
        // No assertion on bar state — indicatif handles its own; just
        // verify the call doesn't panic and clones share state.
        p.skipped_orphan();
        assert_eq!(p.orphans(), 1);
        assert_eq!(clone.orphans(), 1);
    }

    #[test]
    fn finish_is_idempotent() {
        let p = OverallProgress::start("test");
        p.finish();
        // Second call must not panic.
        p.finish();
    }

    #[test]
    fn start_phase_returns_previous_stats() {
        let p = OverallProgress::start("test");
        // First phase → no previous.
        assert!(p.start_phase("hooks").is_none());
        p.tick("a");
        p.tick("b");
        // Second phase → previous stats present.
        let prev = p.start_phase("rules");
        assert!(prev.is_some());
        let (count, _orphans, _dur) = prev.unwrap();
        assert_eq!(count, 2);
    }
}
