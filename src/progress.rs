//! Per-kind progress UX during pull/push.
//!
//! - TTY: `indicatif` spinner → bar with ETA.
//! - Non-TTY (CI, piped): plain `→ kind: …` then `✓ kind: N items, Xs` lines.
//!
//! Drivers receive a thin `&KindProgress` handle. Drop emits the done-line.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// One progress UX surface for one pull/push kind.
pub struct KindProgress {
    inner: Mutex<Inner>,
}

enum Inner {
    Bar {
        kind: String,
        started: Instant,
        bar: indicatif::ProgressBar,
        orphans: AtomicUsize,
        finished: bool,
    },
    Log {
        kind: String,
        started: Instant,
        count: AtomicU64,
        orphans: AtomicUsize,
        finished: bool,
    },
}

impl KindProgress {
    /// Create a new progress surface for `kind`. Auto-detects TTY and
    /// chooses Bar or Log mode. The starting line is emitted immediately.
    /// `kind` is owned (`String`) so dynamic prefixes like
    /// `format!("push envs/{env}")` work alongside the common
    /// `"workspaces"` static literals.
    pub fn start(kind: impl Into<String>) -> Self {
        let kind: String = kind.into();
        if std::io::stderr().is_terminal() {
            let bar = indicatif::ProgressBar::new_spinner();
            bar.set_prefix(kind.clone());
            bar.set_style(
                indicatif::ProgressStyle::with_template("{spinner} {prefix}  listing…")
                    .unwrap(),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(100));
            Self {
                inner: Mutex::new(Inner::Bar {
                    kind,
                    started: Instant::now(),
                    bar,
                    orphans: AtomicUsize::new(0),
                    finished: false,
                }),
            }
        } else {
            eprintln!("→ {kind}: listing…");
            Self {
                inner: Mutex::new(Inner::Log {
                    kind,
                    started: Instant::now(),
                    count: AtomicU64::new(0),
                    orphans: AtomicUsize::new(0),
                    finished: false,
                }),
            }
        }
    }

    /// Switch from spinner-only to bar with denominator `n`. Called once
    /// per kind, after `list_*` returns.
    pub fn set_total(&self, n: u64) {
        let inner = self.inner.lock().unwrap();
        if let Inner::Bar { bar, .. } = &*inner {
            bar.set_length(n);
            bar.set_style(
                indicatif::ProgressStyle::with_template(
                    "{spinner} {prefix}  [{wide_bar}] {pos}/{len}  ETA {eta}",
                )
                .unwrap(),
            );
        }
        // Log mode: nothing to do here — count() is what we report.
    }

    /// Advance by one item.
    pub fn tick(&self) {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { bar, .. } => bar.inc(1),
            Inner::Log { count, .. } => {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Increment the orphan-skipped counter (does not advance the main tick).
    pub fn skipped_orphan(&self) {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { orphans, .. } => {
                orphans.fetch_add(1, Ordering::Relaxed);
            }
            Inner::Log { orphans, .. } => {
                orphans.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Run `f` with the bar paused (in TTY mode), so the bar redraws
    /// cleanly after stderr text. In log mode, `f` runs unchanged.
    pub fn suspend<F: FnOnce()>(&self, f: F) {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { bar, .. } => bar.suspend(f),
            Inner::Log { .. } => f(),
        }
    }

    /// Explicitly finish the bar/log line, emitting the `✓` done-line.
    /// Called from the orchestrator after a successful driver run; on
    /// driver-error the Drop impl skips the done-line so failed kinds
    /// don't show `✓`.
    pub fn finish(self) {
        let mut inner = self.inner.lock().unwrap();
        emit_done(&mut inner);
    }

    /// Read the current orphan-skipped count. Useful for accumulation in
    /// the parent runner's stats struct.
    pub fn orphans(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { orphans, .. } => orphans.load(Ordering::Relaxed),
            Inner::Log { orphans, .. } => orphans.load(Ordering::Relaxed),
        }
    }
}

impl Drop for KindProgress {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        // Suppress done-line on drop when not explicitly finished.
        match &mut *inner {
            Inner::Bar { bar, finished, .. } if !*finished => {
                bar.finish_and_clear();
            }
            Inner::Log { finished, .. } if !*finished => {
                // Nothing — caller bailed without emitting a done-line.
            }
            _ => {}
        }
    }
}

fn emit_done(inner: &mut Inner) {
    match inner {
        Inner::Bar { kind, started, bar, orphans, finished } => {
            *finished = true;
            let dur = started.elapsed();
            let count = bar.position();
            let orphans_n = orphans.load(Ordering::Relaxed);
            bar.finish_and_clear();
            eprintln!("{}", format_done(kind, count, orphans_n, dur));
        }
        Inner::Log { kind, started, count, orphans, finished } => {
            *finished = true;
            let dur = started.elapsed();
            let n = count.load(Ordering::Relaxed);
            let orphans_n = orphans.load(Ordering::Relaxed);
            eprintln!("{}", format_done(kind, n, orphans_n, dur));
        }
    }
}

fn format_done(kind: &str, count: u64, orphans: usize, dur: std::time::Duration) -> String {
    let secs = dur.as_secs_f32();
    if orphans > 0 {
        format!("✓ {kind}: {count} items, {orphans} orphans skipped, {secs:.1}s")
    } else {
        format!("✓ {kind}: {count} items, {secs:.1}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_done_no_orphans() {
        let s = format_done("workspaces", 12, 0, std::time::Duration::from_millis(2100));
        assert_eq!(s, "✓ workspaces: 12 items, 2.1s");
    }

    #[test]
    fn format_done_with_orphans() {
        let s = format_done("queues", 23, 2, std::time::Duration::from_millis(4700));
        assert_eq!(s, "✓ queues: 23 items, 2 orphans skipped, 4.7s");
    }

    #[test]
    fn format_done_zero_items() {
        let s = format_done("hooks", 0, 0, std::time::Duration::from_millis(100));
        assert_eq!(s, "✓ hooks: 0 items, 0.1s");
    }
}
