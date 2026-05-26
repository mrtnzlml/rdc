//! Single-renderer event log used by every `rdc` command.
//!
//! Spec: docs/superpowers/specs/2026-05-21-traditional-log-design.md

/// Closed vocabulary of action verbs. Each variant renders as a fixed-
/// width lowercase token in the action column of every log line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Action {
    // Run lifecycle
    Sync, Deploy, Pull, Push, Diff, Auth, Init, Repair, Upgr,
    // Phase
    Plan, List, Watch,
    // Per-resource
    Post, Patch, Delete, Skip,
    // Disposition
    Done, Fail, Warn, Retry, Info,
    // Watch
    Tick, Idle,
}

impl Action {
    /// Lowercase token, left-padded with spaces to exactly 6 characters.
    /// Used as the action column in every event line; the body column
    /// starts at byte offset 7 (6 + 1 space separator).
    pub fn pad(self) -> &'static str {
        match self {
            Action::Sync   => "sync  ",
            Action::Deploy => "deploy",
            Action::Pull   => "pull  ",
            Action::Push   => "push  ",
            Action::Diff   => "diff  ",
            Action::Auth   => "auth  ",
            Action::Init   => "init  ",
            Action::Repair => "repair",
            Action::Upgr   => "upgr  ",
            Action::Plan   => "plan  ",
            Action::List   => "list  ",
            Action::Watch  => "watch ",
            Action::Post   => "post  ",
            Action::Patch  => "patch ",
            Action::Delete => "delete",
            Action::Skip   => "skip  ",
            Action::Done   => "done  ",
            Action::Fail   => "fail  ",
            Action::Warn   => "warn  ",
            Action::Retry  => "retry ",
            Action::Info   => "info  ",
            Action::Tick   => "tick  ",
            Action::Idle   => "idle  ",
        }
    }
}

/// Color category for an action. Drives which `colorize_*` helper paints
/// the action column.
///
/// The five buckets carry distinct semantics:
/// - `Accent`: lifecycle / command boundaries (bold amber, the Rossum
///   brand color).
/// - `Read`: anything that fetches state without mutating the remote
///   (light/sage green, non-bold).
/// - `Write`: anything that mutates the remote (bold sage green —
///   paired hue with `Read`, brighter weight to signal the side
///   effect).
/// - `Destructive`: irreversible mutations (bold red). Same hue as
///   `Error`, but the action text — `delete` vs `fail` — makes the
///   meaning unambiguous.
/// - `Success` / `Warn` / `Error` / `Dim`: disposition.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ActionColor { Accent, Read, Write, Destructive, Success, Warn, Error, Dim }

impl Action {
    fn color(self) -> ActionColor {
        match self {
            // Lifecycle / command boundaries (bold amber).
            Action::Sync | Action::Deploy | Action::Auth | Action::Init
            | Action::Repair | Action::Upgr | Action::Watch              => ActionColor::Accent,
            // Network reads + local read-only computations (light green).
            Action::Pull | Action::List | Action::Diff | Action::Plan    => ActionColor::Read,
            // Remote mutations: the per-resource events and the push
            // umbrella phase that issues them (bold green).
            Action::Push | Action::Post | Action::Patch                  => ActionColor::Write,
            // Irreversible mutations (bold red).
            Action::Delete                                                => ActionColor::Destructive,
            Action::Done                                                 => ActionColor::Success,
            Action::Warn | Action::Retry                                 => ActionColor::Warn,
            Action::Fail                                                 => ActionColor::Error,
            Action::Skip | Action::Info | Action::Tick | Action::Idle    => ActionColor::Dim,
        }
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;

    #[test]
    fn pad_is_always_six_chars() {
        let variants = [
            Action::Sync, Action::Deploy, Action::Pull, Action::Push,
            Action::Diff, Action::Auth, Action::Init, Action::Repair, Action::Upgr,
            Action::Plan, Action::List, Action::Watch,
            Action::Post, Action::Patch, Action::Delete, Action::Skip,
            Action::Done, Action::Fail, Action::Warn, Action::Retry, Action::Info,
            Action::Tick, Action::Idle,
        ];
        for v in variants {
            assert_eq!(v.pad().len(), 6, "{v:?} pad is not 6 chars: {:?}", v.pad());
        }
    }

    #[test]
    fn action_color_buckets_per_user_spec() {
        // Lifecycle / command boundaries -> bold amber.
        for a in [
            Action::Sync, Action::Deploy, Action::Auth, Action::Init,
            Action::Repair, Action::Upgr, Action::Watch,
        ] {
            assert_eq!(a.color(), ActionColor::Accent, "{a:?} should be Accent");
        }
        // Reads (network + local read-only) -> light green.
        for a in [Action::Pull, Action::List, Action::Diff, Action::Plan] {
            assert_eq!(a.color(), ActionColor::Read, "{a:?} should be Read");
        }
        // Writes -> bold green.
        for a in [Action::Push, Action::Post, Action::Patch] {
            assert_eq!(a.color(), ActionColor::Write, "{a:?} should be Write");
        }
        // Destructive -> bold red.
        assert_eq!(Action::Delete.color(), ActionColor::Destructive);
        // Disposition unchanged.
        assert_eq!(Action::Done.color(), ActionColor::Success);
        assert_eq!(Action::Warn.color(), ActionColor::Warn);
        assert_eq!(Action::Retry.color(), ActionColor::Warn);
        assert_eq!(Action::Fail.color(), ActionColor::Error);
        for a in [Action::Skip, Action::Info, Action::Tick, Action::Idle] {
            assert_eq!(a.color(), ActionColor::Dim, "{a:?} should be Dim");
        }
    }

    #[test]
    fn pad_is_lowercase_ascii_with_trailing_spaces() {
        for v in [Action::Sync, Action::Push, Action::Delete, Action::Upgr] {
            let s = v.pad();
            let trimmed = s.trim_end();
            assert!(trimmed.chars().all(|c| c.is_ascii_lowercase()),
                "{v:?} not lowercase ascii: {s:?}");
            // Padding is on the right (left-aligned).
            assert!(!s.starts_with(' '), "{v:?} has leading space: {s:?}");
        }
    }
}

/// UTC HH:MM:SS — small standalone formatter so we don't add a `chrono`
/// dep. Mirrors the helper that previously lived in `cli::sync::watch`.
pub(crate) fn now_hhmmss() -> String {
    format_hhmmss(std::time::SystemTime::now())
}

fn format_hhmmss(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_today = secs % 86400;
    let h = secs_today / 3600;
    let m = (secs_today % 3600) / 60;
    let s = secs_today % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod time_tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn format_hhmmss_at_epoch_is_midnight_utc() {
        assert_eq!(format_hhmmss(UNIX_EPOCH), "00:00:00");
    }

    #[test]
    fn format_hhmmss_wraps_modulo_day() {
        // Epoch + 1 day + 1h2m3s should print 01:02:03.
        let t = UNIX_EPOCH + Duration::from_secs(86400 + 3723);
        assert_eq!(format_hhmmss(t), "01:02:03");
    }

    #[test]
    fn format_hhmmss_pads_single_digits() {
        let t = UNIX_EPOCH + Duration::from_secs(5);
        assert_eq!(format_hhmmss(t), "00:00:05");
    }
}

use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::cli::resolve::ColorMode;

/// Single-renderer event log. Threaded as `Arc<Log>` through every
/// command. Lines are emitted to stderr.
pub struct Log {
    color: ColorMode,
    /// True only when constructed by `Log::new` and stderr is a real TTY.
    /// Gates in-place status updates (`tick_status`); on non-TTY the calls
    /// are no-ops so log output stays scrollback-friendly.
    is_tty: bool,
    state: Mutex<LogState>,
    /// Test-only clock override. Production code reads
    /// `std::time::SystemTime::now()`.
    #[cfg(test)]
    fixed_time: Option<std::time::SystemTime>,
}

struct LogState {
    out: Box<dyn Write + Send>,
    /// True while an in-place status line is currently drawn on the
    /// terminal. The next `event` / `block` call clears it before
    /// writing its own content.
    status_active: bool,
}

impl Log {
    /// Construct a Log that writes to stderr.
    pub fn new(color: ColorMode) -> Arc<Self> {
        use std::io::IsTerminal;
        let is_tty = std::io::stderr().is_terminal();
        Arc::new(Self {
            color,
            is_tty,
            state: Mutex::new(LogState {
                out: Box::new(std::io::stderr()),
                status_active: false,
            }),
            #[cfg(test)]
            fixed_time: None,
        })
    }

    /// Construct a Log that writes into the given sink. Test-only.
    #[cfg(test)]
    fn for_test(color: ColorMode, sink: Box<dyn Write + Send>) -> Arc<Self> {
        Arc::new(Self {
            color,
            is_tty: false,
            state: Mutex::new(LogState { out: sink, status_active: false }),
            fixed_time: None,
        })
    }

    /// Construct a Log that writes into the given sink and reports a
    /// fixed time on every `event()` call. Test-only.
    #[cfg(test)]
    fn for_test_with_time(
        color: ColorMode,
        sink: Box<dyn Write + Send>,
        time: std::time::SystemTime,
    ) -> Arc<Self> {
        Arc::new(Self {
            color,
            is_tty: false,
            state: Mutex::new(LogState { out: sink, status_active: false }),
            fixed_time: Some(time),
        })
    }

    /// Like `for_test_with_time` but with `is_tty = true` so
    /// `tick_status` actually writes. Test-only.
    #[cfg(test)]
    fn for_test_tty_with_time(
        color: ColorMode,
        sink: Box<dyn Write + Send>,
        time: std::time::SystemTime,
    ) -> Arc<Self> {
        Arc::new(Self {
            color,
            is_tty: true,
            state: Mutex::new(LogState { out: sink, status_active: false }),
            fixed_time: Some(time),
        })
    }

    fn now_string(&self) -> String {
        #[cfg(test)]
        if let Some(t) = self.fixed_time {
            return format_hhmmss(t);
        }
        now_hhmmss()
    }

    fn render_line(&self, action: Action, body: &str) -> String {
        use crate::cli::resolve::{
            colorize_dim, colorize_error, colorize_final_ok, colorize_header, colorize_success,
            colorize_warning,
        };
        let time = self.now_string();
        let raw_pad = action.pad();
        let action_col = match action.color() {
            // bold amber
            ActionColor::Accent      => colorize_header(raw_pad, self.color),
            // light/sage green (non-bold), shared with Success disposition
            ActionColor::Read        => colorize_success(raw_pad, self.color),
            // bold sage green — paired with Read but visibly louder
            ActionColor::Write       => colorize_final_ok(raw_pad, self.color),
            // bold red, same hue as Error (text disambiguates)
            ActionColor::Destructive => colorize_error(raw_pad, self.color),
            ActionColor::Success     => colorize_success(raw_pad, self.color),
            ActionColor::Warn        => colorize_warning(raw_pad, self.color),
            ActionColor::Error       => colorize_error(raw_pad, self.color),
            ActionColor::Dim         => colorize_dim(raw_pad, self.color),
        };
        let time_col = colorize_dim(&time, self.color);
        format!("{time_col} {action_col} {body}")
    }

    /// Emit one timestamped event line:
    /// `HH:MM:SS <action> <body>\n`.
    pub fn event(&self, action: Action, body: &str) {
        let line = self.render_line(action, body);
        let mut state = self.state.lock().unwrap();
        if state.status_active {
            // Clear the in-place status line so the event prints fresh.
            let _ = state.out.write_all(b"\r\x1b[K");
            state.status_active = false;
        }
        let _ = state.out.write_all(line.as_bytes());
        let _ = state.out.write_all(b"\n");
        let _ = state.out.flush();
    }

    /// Update the in-place status line. TTY only; a no-op on non-TTY so
    /// scrollback / `tee` capture stay clean. The next `event` or `block`
    /// call clears whatever this drew.
    pub fn tick_status(&self, action: Action, body: &str) {
        if !self.is_tty { return; }
        let line = self.render_line(action, body);
        let mut state = self.state.lock().unwrap();
        let _ = state.out.write_all(b"\r");
        let _ = state.out.write_all(line.as_bytes());
        let _ = state.out.write_all(b"\x1b[K");
        let _ = state.out.flush();
        state.status_active = true;
    }

    /// Commit any in-place status line with a trailing newline. Most
    /// callers don't need this — `event` and `block` clear automatically.
    /// Useful at shutdown to leave the terminal cursor on a fresh line.
    pub fn finish_status(&self) {
        let mut state = self.state.lock().unwrap();
        if state.status_active {
            let _ = state.out.write_all(b"\n");
            let _ = state.out.flush();
            state.status_active = false;
        }
    }

    /// Emit a multi-line block verbatim, without timestamp or action
    /// column. Used for plan / dry-run bodies, JSON dumps, and inline
    /// prompt UI. A trailing newline is added if missing.
    pub fn block(&self, body: &str) {
        let mut state = self.state.lock().unwrap();
        if state.status_active {
            let _ = state.out.write_all(b"\r\x1b[K");
            state.status_active = false;
        }
        let _ = state.out.write_all(body.as_bytes());
        if !body.ends_with('\n') {
            let _ = state.out.write_all(b"\n");
        }
        let _ = state.out.flush();
    }

    /// Run an inline interactive prompt (auth refresh, conflict resolver,
    /// destructive delete gate). Currently flushes pending output then
    /// runs the closure — kept as a method so callsites express intent
    /// and we can add suspending later without rewriting them.
    pub fn with_prompt<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let mut state = self.state.lock().unwrap();
        if state.status_active {
            let _ = state.out.write_all(b"\r\x1b[K");
            state.status_active = false;
        }
        let _ = state.out.flush();
        drop(state);
        f()
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, UNIX_EPOCH};

    /// In-memory sink shared by tests.
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn event_line_shape() {
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14), // 12:01:14
        );
        log.event(Action::Push, "rule/finance-totals PATCH 412ms");
        assert_eq!(buf.text(), "12:01:14 push   rule/finance-totals PATCH 412ms\n");
    }

    #[test]
    fn body_column_aligned_across_actions() {
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        for a in [Action::Sync, Action::Push, Action::Patch, Action::Delete, Action::Upgr] {
            log.event(a, "x");
        }
        let text = buf.text();
        for line in text.lines() {
            // "HH:MM:SS " is 9 chars, action col is 6 chars, separator 1 char = 16 → body starts at byte 16.
            assert_eq!(&line[16..17], "x", "body misaligned in {line:?}");
        }
    }

    #[test]
    fn block_emits_verbatim_then_newline() {
        let buf = Buf::default();
        let log = Log::for_test(ColorMode::Plain, Box::new(buf.clone()));
        log.block("--- preview ---\n  line one\n  line two");
        assert_eq!(buf.text(), "--- preview ---\n  line one\n  line two\n");
    }

    #[test]
    fn block_does_not_double_newline() {
        let buf = Buf::default();
        let log = Log::for_test(ColorMode::Plain, Box::new(buf.clone()));
        log.block("one\ntwo\n");
        assert_eq!(buf.text(), "one\ntwo\n");
    }

    #[test]
    fn with_prompt_runs_closure_and_returns_value() {
        let buf = Buf::default();
        let log = Log::for_test(ColorMode::Plain, Box::new(buf.clone()));
        let got = log.with_prompt(|| 42);
        assert_eq!(got, 42);
    }

    #[test]
    fn plain_color_produces_no_ansi_escapes() {
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        for a in [Action::Sync, Action::Push, Action::Done, Action::Warn, Action::Fail, Action::Skip] {
            log.event(a, "x");
        }
        let text = buf.text();
        assert!(!text.contains('\x1b'), "ANSI escape leaked under Plain: {text:?}");
        // Body still aligns to column 16.
        for line in text.lines() {
            assert_eq!(&line[16..17], "x", "body misaligned: {line:?}");
        }
    }

    #[test]
    fn tick_status_is_noop_on_non_tty() {
        let buf = Buf::default();
        let log = Log::for_test_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        log.tick_status(Action::Watch, "next sync in 33s ▰▱▱");
        assert_eq!(buf.text(), "", "tick_status must not emit on non-TTY");
    }

    #[test]
    fn tick_status_writes_in_place_on_tty() {
        let buf = Buf::default();
        let log = Log::for_test_tty_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        log.tick_status(Action::Watch, "next sync in 33s ▰▱▱");
        // Carriage return, content, clear-to-EOL — no newline.
        assert_eq!(
            buf.text(),
            "\r12:01:14 watch  next sync in 33s ▰▱▱\x1b[K"
        );
    }

    #[test]
    fn event_clears_active_status_line() {
        let buf = Buf::default();
        let log = Log::for_test_tty_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        log.tick_status(Action::Watch, "next sync in 33s ▰▱▱");
        log.event(Action::Sync, "start envs/test");
        let text = buf.text();
        // The event must arrive after a clear sequence (\r\x1b[K), and
        // end with a newline of its own.
        assert!(
            text.contains("\r\x1b[K12:01:14 sync   start envs/test\n"),
            "missing clear-then-event sequence: {text:?}"
        );
    }

    #[test]
    fn finish_status_commits_with_newline() {
        let buf = Buf::default();
        let log = Log::for_test_tty_with_time(
            ColorMode::Plain,
            Box::new(buf.clone()),
            UNIX_EPOCH + Duration::from_secs(12 * 3600 + 1 * 60 + 14),
        );
        log.tick_status(Action::Watch, "next sync in 5s ▰▰▰");
        log.finish_status();
        assert!(buf.text().ends_with('\n'), "finish_status must add a newline: {:?}", buf.text());
        // Second call is a no-op.
        let len_after_first = buf.text().len();
        log.finish_status();
        assert_eq!(buf.text().len(), len_after_first, "finish_status must be idempotent");
    }
}
