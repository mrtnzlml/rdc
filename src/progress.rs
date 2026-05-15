//! Per-run UX during pull/push/deploy/sync — colored event log with
//! per-line spinners while individual operations are in flight.
//!
//! Spec: docs/superpowers/specs/2026-05-15-progress-log-design.md

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
}

impl ProgressLog {
    /// Print the header line and return the handle.
    /// `title` shows as the first line (`→ rdc sync test`).
    pub fn start(title: impl Into<String>) -> Arc<Self> {
        let title: String = title.into();
        let tty = std::io::stderr().is_terminal();
        let color = crate::cli::resolve::detect_color_mode(false);
        let mp = indicatif::MultiProgress::new();
        // Header line goes through `eprintln!` (not the MP) because there's
        // no in-flight item to animate — it's a stable line at the top.
        let header = format!("→ {title}");
        eprintln!("{}", crate::cli::resolve::colorize_header(&header, color));
        Arc::new(Self {
            inner: Mutex::new(LogInner {
                title,
                mp,
                tty,
                color,
                current_phase: None,
                finished: false,
            }),
        })
    }

    /// Start a labelled section. The label appears on its own line.
    /// Subsequent `item()` calls (on the returned `Phase`) are indented
    /// under it. Calling `phase()` again finalizes the previous section's
    /// blank-line padding and starts a fresh one.
    pub fn phase(self: &Arc<Self>, label: impl Into<String>) -> Phase {
        let label: String = label.into();
        {
            let mut inner = self.inner.lock().unwrap();
            // Blank line before each subsequent phase.
            if inner.current_phase.is_some() {
                eprintln!();
            }
            inner.current_phase = Some(label.clone());
            let styled = crate::cli::resolve::colorize_header(&label, inner.color);
            eprintln!("{styled}");
        }
        Phase {
            log: self.clone(),
            label,
        }
    }

    /// Print a free-standing line that doesn't fit the spinner shape.
    /// Used by HTTP retry warnings from the API layer — the message needs
    /// to appear without corrupting any in-flight spinner draw. In TTY
    /// mode this goes through `MultiProgress::println` which redraws
    /// cleanly above the bars; otherwise it falls through to `eprintln!`.
    pub fn println(&self, msg: impl AsRef<str>) {
        let msg = msg.as_ref();
        let inner = self.inner.lock().unwrap();
        if inner.tty {
            let _ = inner.mp.println(msg);
        } else {
            eprintln!("{msg}");
        }
    }

    /// Final summary line on success. Idempotent — calling twice is a no-op.
    pub fn finish(self: &Arc<Self>, summary: impl Into<String>) {
        let summary: String = summary.into();
        let mut inner = self.inner.lock().unwrap();
        if inner.finished {
            return;
        }
        inner.finished = true;
        // Blank line before the final line if any phase was emitted.
        if inner.current_phase.is_some() {
            eprintln!();
        }
        let line = format!("✔ {summary}");
        let styled = crate::cli::resolve::colorize_final_ok(&line, inner.color);
        eprintln!("{styled}");
    }

    /// Final summary line on error. Idempotent.
    pub fn finish_err(self: &Arc<Self>, msg: impl Into<String>) {
        let msg: String = msg.into();
        let mut inner = self.inner.lock().unwrap();
        if inner.finished {
            return;
        }
        inner.finished = true;
        if inner.current_phase.is_some() {
            eprintln!();
        }
        let line = format!("✗ {msg}");
        let styled = crate::cli::resolve::colorize_error(&line, inner.color);
        eprintln!("{styled}");
    }
}

/// One section of the run (`listing remote`, `scanning local`, `pushing`).
/// Items inside the section render indented; spinners attach to items.
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
                ProgressStyle::with_template("  {spinner} {msg}")
                    .unwrap()
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(80));
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
    /// `classifying`). Renders as `  <content>`.
    pub fn line(&self, content: impl Into<String>) {
        let content: String = content.into();
        let _inner = self.log.inner.lock().unwrap();
        eprintln!("  {content}");
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

    /// Resolve with a ✓ and optional summary.
    pub fn finish_ok(mut self, summary: impl Into<String>) {
        if self.resolved { return; }
        self.resolved = true;
        let summary: String = summary.into();
        let elapsed = self.started.elapsed();
        let line = format_final_line("✓", &self.name, &summary, elapsed, self.color);
        if self.tty {
            self.bar.finish_with_message(line);
        } else {
            eprintln!("  {line}");
        }
    }

    /// Resolve with a ⚠️ and one-line warning.
    pub fn finish_warn(mut self, msg: impl Into<String>) {
        if self.resolved { return; }
        self.resolved = true;
        let msg: String = msg.into();
        let elapsed = self.started.elapsed();
        let line = format_final_line("⚠\u{FE0F}", &self.name, &msg, elapsed, self.color);
        if self.tty {
            self.bar.finish_with_message(line);
        } else {
            eprintln!("  {line}");
        }
    }

    /// Resolve with a ✗ and one-line error.
    #[allow(dead_code)]
    pub fn finish_err(mut self, msg: impl Into<String>) {
        if self.resolved { return; }
        self.resolved = true;
        let msg: String = msg.into();
        let elapsed = self.started.elapsed();
        let line = format_final_line("✗", &self.name, &msg, elapsed, self.color);
        if self.tty {
            self.bar.finish_with_message(line);
        } else {
            eprintln!("  {line}");
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        self.resolved = true;
        let line = format!("⊘ {} (cancelled)", self.name);
        if self.tty {
            self.bar.finish_with_message(line);
        } else {
            eprintln!("  {line}");
        }
    }
}

/// Build one resolved line. Public so tests can poke at it without
/// constructing a full Spinner.
fn format_final_line(
    glyph: &str,
    name: &str,
    summary: &str,
    elapsed: std::time::Duration,
    color: crate::cli::resolve::ColorMode,
) -> String {
    let body = if summary.is_empty() {
        format!("{glyph} {name}")
    } else {
        format!("{glyph} {name} {summary}")
    };
    let body = if elapsed > std::time::Duration::from_millis(200) {
        format!("{body} ({:.1}s)", elapsed.as_secs_f32())
    } else {
        body
    };
    match glyph {
        "✓" => crate::cli::resolve::colorize_success(&body, color),
        "⚠\u{FE0F}" => crate::cli::resolve::colorize_warning(&body, color),
        "✗" => crate::cli::resolve::colorize_error(&body, color),
        _ => body,
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;
    use std::time::Duration;
    use crate::cli::resolve::ColorMode;

    #[test]
    fn format_final_line_short_op_omits_elapsed() {
        let line = format_final_line("✓", "workspaces", "4", Duration::from_millis(40), ColorMode::Plain);
        assert_eq!(line, "✓ workspaces 4");
    }

    #[test]
    fn format_final_line_long_op_includes_elapsed() {
        let line = format_final_line("✓", "schemas", "24", Duration::from_millis(1400), ColorMode::Plain);
        assert_eq!(line, "✓ schemas 24 (1.4s)");
    }

    #[test]
    fn format_final_line_empty_summary_just_glyph_and_name() {
        let line = format_final_line("⚠\u{FE0F}", "hooks/x", "", Duration::from_millis(50), ColorMode::Plain);
        assert_eq!(line, "⚠\u{FE0F} hooks/x");
    }
}
