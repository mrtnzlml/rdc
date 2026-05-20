//! Per-run UX during pull/push/deploy/sync: colored event log with
//! per-line spinners while individual operations are in flight.
//!
//! Spec: docs/superpowers/specs/2026-05-15-progress-log-design.md
//!
//! Every line is routed through `MultiProgress::println` (TTY mode) so
//! warnings, info notes, and phase headers cleanly suspend any active
//! spinner, print above it, and let it resume. In non-TTY mode lines
//! fall through to plain `eprintln!`. All glyphs are ASCII to keep
//! column widths predictable across terminals.

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use indicatif::{ProgressBar, ProgressStyle};

/// Optional reference to the run-wide progress log. `None` when no run
/// is active (e.g. `rdc auth`, `rdc diff`). Threaded through API client
/// methods so retry warnings can render above any in-flight spinner.
pub type ProgressHandle = Option<Arc<ProgressLog>>;

/// Run-wide handle for the event-log UX. Created once at the top of a
/// pull/push/deploy/sync run; cloneable into per-driver scopes.
pub struct ProgressLog {
    inner: Mutex<LogInner>,
}

struct LogInner {
    #[allow(dead_code)]
    title: String,
    /// MultiProgress so the active spinner doesn't tear the surrounding
    /// stderr writes. Lives for the duration of the run.
    mp: indicatif::MultiProgress,
    /// Whether stderr is a real TTY (drives spinner animation vs plain lines).
    tty: bool,
    /// Color mode resolved at construction; cheap to copy.
    color: crate::cli::resolve::ColorMode,
    /// The most-recently-printed phase label, used so `Phase::item` knows
    /// the right indent context. None before the first `phase()` call.
    current_phase: Option<String>,
    /// Whether `finish` / `finish_err` has been called.
    finished: bool,
    /// Weak self-reference so `&self` trait methods can re-acquire the
    /// Arc<ProgressLog> needed by the existing `phase` / `finish` methods.
    self_weak: std::sync::Weak<ProgressLog>,
}

impl ProgressLog {
    /// Construct the run-wide handle. No header line is emitted: the user
    /// just typed the command, so echoing it back is noise. The `title`
    /// field is retained on `LogInner` for potential future use (e.g. an
    /// informative final summary) but never printed by `start`.
    pub fn start(title: impl Into<String>) -> Arc<Self> {
        let title: String = title.into();
        let tty = std::io::stderr().is_terminal();
        let color = crate::cli::resolve::detect_color_mode(false);
        let mp = indicatif::MultiProgress::new();
        Arc::new_cyclic(|weak| Self {
            inner: Mutex::new(LogInner {
                title,
                mp,
                tty,
                color,
                current_phase: None,
                finished: false,
                self_weak: weak.clone(),
            }),
        })
    }

    /// Re-acquire `Arc<Self>` from inside a `&self` trait method. Panics
    /// if called after the last external `Arc<Self>` has been dropped —
    /// in practice, all trait calls happen while the dispatcher holds
    /// an `Arc<dyn SyncRenderer>`, so this is safe.
    fn clone_arc(&self) -> Arc<Self> {
        self.inner.lock().unwrap().self_weak.upgrade()
            .expect("ProgressLog dropped while trait method was running")
    }

    /// Start a labelled section. The label appears on its own line,
    /// bold/colored so it reads as a section break, with a blank line
    /// above it (except the first). Subsequent `item()` / `line()` /
    /// `warn()` calls render flush-left under the header. Calling
    /// `phase()` again starts a fresh section.
    pub fn phase(self: &Arc<Self>, label: impl Into<String>) -> Phase {
        let label: String = label.into();
        {
            let mut inner = self.inner.lock().unwrap();
            let styled = crate::cli::resolve::colorize_header(&label, inner.color);
            // Blank line before each subsequent phase. Route both the
            // blank line and the header through `mp.println` so any
            // in-flight spinner (from a prior phase that didn't fully
            // resolve before re-entering) is properly suspended.
            if inner.current_phase.is_some() {
                if inner.tty {
                    let _ = inner.mp.println("");
                } else {
                    eprintln!();
                }
            }
            if inner.tty {
                let _ = inner.mp.println(&styled);
            } else {
                eprintln!("{styled}");
            }
            inner.current_phase = Some(label.clone());
        }
        Phase {
            log: self.clone(),
            label,
        }
    }

    /// Print a free-standing line that doesn't fit the spinner shape.
    /// The message is written verbatim with no extra indent. Reserve this
    /// for full-width lines (final summaries, blank padding). Phase-scoped
    /// notices should use [`Self::warn`] instead so they line up under the
    /// active phase. In TTY mode this goes through `MultiProgress::println`
    /// which redraws cleanly above the bars; otherwise it falls through to
    /// `eprintln!`.
    pub fn println(&self, msg: impl AsRef<str>) {
        let msg = msg.as_ref();
        let inner = self.inner.lock().unwrap();
        if inner.tty {
            let _ = inner.mp.println(msg);
        } else {
            eprintln!("{msg}");
        }
    }

    /// Emit a notice (warning, retry, conflict note) under the active
    /// phase. Renders flush-left, matching [`Phase::item`] / [`Phase::line`]
    /// and the surrounding phase items. Routes through
    /// `MultiProgress::println` so any in-flight spinner is suspended for
    /// the write; falls through to `eprintln!` in non-TTY mode.
    pub fn warn(&self, msg: impl AsRef<str>) {
        let msg = msg.as_ref();
        let inner = self.inner.lock().unwrap();
        if inner.tty {
            let _ = inner.mp.println(msg);
        } else {
            eprintln!("{msg}");
        }
    }

    /// Final summary line on success. Idempotent: calling twice is a no-op.
    pub fn finish(self: &Arc<Self>, summary: impl Into<String>) {
        let summary: String = summary.into();
        let mut inner = self.inner.lock().unwrap();
        if inner.finished {
            return;
        }
        inner.finished = true;
        let line = format!("DONE: {summary}");
        let styled = crate::cli::resolve::colorize_final_ok(&line, inner.color);
        // Blank line before the final line if any phase was emitted.
        if inner.current_phase.is_some() {
            if inner.tty {
                let _ = inner.mp.println("");
            } else {
                eprintln!();
            }
        }
        if inner.tty {
            let _ = inner.mp.println(&styled);
        } else {
            eprintln!("{styled}");
        }
    }

    /// Final summary line on error. Idempotent.
    pub fn finish_err(self: &Arc<Self>, msg: impl Into<String>) {
        let msg: String = msg.into();
        let mut inner = self.inner.lock().unwrap();
        if inner.finished {
            return;
        }
        inner.finished = true;
        let line = format!("FAIL: {msg}");
        let styled = crate::cli::resolve::colorize_error(&line, inner.color);
        if inner.current_phase.is_some() {
            if inner.tty {
                let _ = inner.mp.println("");
            } else {
                eprintln!();
            }
        }
        if inner.tty {
            let _ = inner.mp.println(&styled);
        } else {
            eprintln!("{styled}");
        }
    }
}

/// One section of the run (`listing remote`, `scanning local`, `pushing`).
/// Items inside the section render flush-left; spinners attach to items.
pub struct Phase {
    log: Arc<ProgressLog>,
    #[allow(dead_code)]
    label: String,
}

impl Phase {
    /// Start a single in-flight item. The line begins with a spinner and
    /// the given name; the caller resolves it via `Spinner::finish_*`.
    pub fn item(&self, name: impl Into<String>) -> Spinner {
        let name: String = name.into();
        let (mp, tty, color) = {
            let inner = self.log.inner.lock().unwrap();
            (inner.mp.clone(), inner.tty, inner.color)
        };
        let bar = if tty {
            let bar = mp.add(ProgressBar::new_spinner());
            bar.set_style(
                ProgressStyle::with_template("{spinner} {msg}")
                    .unwrap()
                    .tick_strings(&["|", "/", "-", "\\"]),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            bar.set_message(name.clone());
            bar
        } else {
            let bar = mp.add(ProgressBar::hidden());
            bar.set_message(name.clone());
            bar
        };
        Spinner {
            bar,
            name,
            color,
            tty,
            started: std::time::Instant::now(),
            resolved: false,
        }
    }

    /// One-shot summary line without a spinner. Used for steps where the
    /// work is so fast a spinner would flicker (`scanning local`,
    /// `classifying`). Renders flush-left and routes through
    /// `MultiProgress::println` so any active spinner is suspended for
    /// the duration of the write.
    pub fn line(&self, content: impl Into<String>) {
        let content: String = content.into();
        let inner = self.log.inner.lock().unwrap();
        if inner.tty {
            let _ = inner.mp.println(&content);
        } else {
            eprintln!("{content}");
        }
    }
}

/// One in-flight item line. Hold while work runs; call `finish_*` to
/// resolve. If dropped without resolving, finalizes as "(cancelled)".
pub struct Spinner {
    bar: ProgressBar,
    name: String,
    color: crate::cli::resolve::ColorMode,
    tty: bool,
    started: std::time::Instant,
    resolved: bool,
}

impl Spinner {
    /// Update the in-flight message (rare; most callers don't need this).
    #[allow(dead_code)]
    pub fn set_message(&self, msg: impl Into<String>) {
        self.bar.set_message(msg.into());
    }

    /// Resolve with an `[ok]` and optional summary.
    pub fn finish_ok(mut self, summary: impl Into<String>) {
        if self.resolved { return; }
        self.resolved = true;
        let summary: String = summary.into();
        let elapsed = self.started.elapsed();
        let line = format_final_line("[ok]", &self.name, &summary, elapsed, self.color);
        if self.tty {
            // Clear the bar (drops the `{spinner}` template so no frozen
            // animation frame is left behind), then commit a permanent
            // line above the active draw region.
            self.bar.finish_and_clear();
            self.bar.println(line);
        } else {
            eprintln!("{line}");
        }
    }

    /// Resolve with a `!` warning marker and one-line warning text.
    pub fn finish_warn(mut self, msg: impl Into<String>) {
        if self.resolved { return; }
        self.resolved = true;
        let msg: String = msg.into();
        let elapsed = self.started.elapsed();
        let line = format_final_line("!", &self.name, &msg, elapsed, self.color);
        if self.tty {
            self.bar.finish_and_clear();
            self.bar.println(line);
        } else {
            eprintln!("{line}");
        }
    }

    /// Resolve with a `[fail]` marker and one-line error.
    #[allow(dead_code)]
    pub fn finish_err(mut self, msg: impl Into<String>) {
        if self.resolved { return; }
        self.resolved = true;
        let msg: String = msg.into();
        let elapsed = self.started.elapsed();
        let line = format_final_line("[fail]", &self.name, &msg, elapsed, self.color);
        if self.tty {
            self.bar.finish_and_clear();
            self.bar.println(line);
        } else {
            eprintln!("{line}");
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        self.resolved = true;
        let line = format!("(cancelled) {}", self.name);
        if self.tty {
            self.bar.finish_and_clear();
            self.bar.println(line);
        } else {
            eprintln!("{line}");
        }
    }
}

/// Build one resolved line. Public so tests can poke at it without
/// constructing a full Spinner.
fn format_final_line(
    marker: &str,
    name: &str,
    summary: &str,
    elapsed: std::time::Duration,
    color: crate::cli::resolve::ColorMode,
) -> String {
    let body = if summary.is_empty() {
        format!("{marker} {name}")
    } else {
        format!("{marker} {name} {summary}")
    };
    let body = if elapsed > std::time::Duration::from_millis(200) {
        format!("{body} ({:.1}s)", elapsed.as_secs_f32())
    } else {
        body
    };
    match marker {
        "[ok]" => crate::cli::resolve::colorize_success(&body, color),
        "!" => crate::cli::resolve::colorize_warning(&body, color),
        "[fail]" => crate::cli::resolve::colorize_error(&body, color),
        _ => body,
    }
}

use crate::progress::{ResourceOp, ResourceOutcome, Severity, SyncRenderer};
use crate::cli::sync::classify::ClassifiedItem;

impl SyncRenderer for ProgressLog {
    fn phase(&self, label: &str) {
        // The existing `phase(self: &Arc<Self>, ...)` returns a `Phase`
        // handle. For the trait surface, we re-acquire the Arc, emit
        // the header line, and drop the handle. Items inside the
        // section come through subsequent `warn_line` calls.
        let arc = self.clone_arc();
        let _phase = arc.phase(label.to_string());
    }

    fn warn_line(&self, msg: &str) {
        self.warn(msg);
    }

    fn resource_started(&self, _kind: &str, _slug: &str, _op: ResourceOp) {
        // No-op for the line-based log. Per-resource events are a
        // grid-only concern.
    }

    fn resource_finished(&self, _kind: &str, _slug: &str, _outcome: ResourceOutcome) {
        // No-op for the line-based log.
    }

    fn ingest_classification(&self, _items: &[ClassifiedItem]) {
        // No-op for the line-based log. The dry-run plan enumeration
        // and the per-driver `[ok] <kind> <count>` lines already give
        // the log-mode user a full picture.
    }

    fn banner(&self, severity: Severity, msg: &str) {
        match severity {
            Severity::Info => self.println(msg),
            Severity::Warn | Severity::Error => self.warn(msg),
        }
    }

    fn with_prompt(&self, f: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
        // `MultiProgress::println` already suspends any in-flight
        // spinner cleanly. Inline prompt reads via `eprint!` /
        // `read_line` don't need extra coordination.
        f()
    }

    fn finish_ok(&self, summary: &str) {
        let arc = self.clone_arc();
        arc.finish(summary.to_string());
    }

    fn finish_err(&self, msg: &str) {
        let arc = self.clone_arc();
        arc.finish_err(msg.to_string());
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;
    use std::time::Duration;
    use crate::cli::resolve::ColorMode;

    #[test]
    fn format_final_line_short_op_omits_elapsed() {
        let line = format_final_line("[ok]", "workspaces", "4", Duration::from_millis(40), ColorMode::Plain);
        assert_eq!(line, "[ok] workspaces 4");
    }

    #[test]
    fn format_final_line_long_op_includes_elapsed() {
        let line = format_final_line("[ok]", "schemas", "24", Duration::from_millis(1400), ColorMode::Plain);
        assert_eq!(line, "[ok] schemas 24 (1.4s)");
    }

    #[test]
    fn format_final_line_empty_summary_just_marker_and_name() {
        let line = format_final_line("!", "hooks/x", "", Duration::from_millis(50), ColorMode::Plain);
        assert_eq!(line, "! hooks/x");
    }

    /// Regression: the final `[ok]` line is driven by the spinner's
    /// constructor-time `name`, not by whatever transient `set_message`
    /// was last called with. The "schemas + inboxes (N/M)" counter-bug
    /// (M-number-suppressed: `[ok] schemas + inboxes (0/25) 25 fetched`)
    /// was caused by passing the counter as the constructor name; the
    /// fix is to construct with the bare base name and use `set_message`
    /// for the in-flight counter only. This test pins the formatter
    /// contract: given the base name + summary, the final line carries
    /// neither a `(N/M)` artifact nor any other transient text.
    #[test]
    fn format_final_line_uses_base_name_not_transient_message() {
        let line = format_final_line(
            "[ok]",
            "schemas + inboxes",
            "25 fetched",
            Duration::from_millis(1700),
            ColorMode::Plain,
        );
        assert_eq!(line, "[ok] schemas + inboxes 25 fetched (1.7s)");
        assert!(!line.contains("(0/"), "leftover counter in final line: {line:?}");
    }

    /// Regression: the formatter never PREFIXES the line with a spinner
    /// tick glyph (`|`, `/`, `-`, `\`). The fix for the "random spinner
    /// frame leak" issue relies on `format_final_line` producing a line
    /// whose first non-space content is the marker (`[ok]`, `!`, `[fail]`),
    /// never a tick. Spinner glyphs are rendered by indicatif's template,
    /// not by this formatter; if any frame ever sneaked into the final
    /// line it would precede the marker.
    #[test]
    fn format_final_line_never_starts_with_spinner_glyph() {
        let cases = [
            ("[ok]", "schemas", "24", Duration::from_millis(1400)),
            ("!", "hooks/x", "warn", Duration::from_millis(50)),
            ("[fail]", "queues/y", "boom", Duration::from_millis(2000)),
            ("[ok]", "workspaces", "", Duration::from_millis(40)),
        ];
        let spinner_glyphs = ['|', '/', '-', '\\'];
        for (marker, name, summary, elapsed) in cases {
            let line = format_final_line(marker, name, summary, elapsed, ColorMode::Plain);
            let first = line.chars().next().expect("non-empty line");
            assert!(
                !spinner_glyphs.contains(&first),
                "final line {line:?} starts with spinner glyph {first:?}"
            );
            assert!(
                line.starts_with(marker),
                "final line {line:?} must start with marker {marker:?}"
            );
        }
    }
}
