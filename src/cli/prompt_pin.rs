//! Byte-exact pins for every prompt rdc can put in front of a user.
//!
//! One file per prompt under `testdata/prompt_pins/`, so the whole
//! interactive surface is readable — and reviewable in `git diff` — without
//! running the binary. A test drives the real prompt function, captures what
//! it wrote, and compares it here.
//!
//! Deliberately a plain file rather than a snapshot crate. `insta` trims the
//! trailing whitespace at the end of a snapshot, so `"… [a] abort > "` is
//! stored as `"… [a] abort >"` AND still compares equal after the trailing
//! space is deleted from the code — the space that leaves the cursor one
//! column clear of the question is exactly what these pins exist to protect.
//!
//! Two shapes live in the directory:
//!
//! - **Terminal prompts** — the literal bytes the prompt wrote, `"> "` and
//!   all. Everything that takes a `Write` sink pins this way.
//! - **`inquire` prompts** — question on the first line, then one option per
//!   line indented by two spaces. `inquire` draws its own widget straight to
//!   the terminal with no sink to capture, so the pin records the strings rdc
//!   composes and nothing of inquire's chrome.
//!
//! A pin is a TRANSCRIPT: what the prompt wrote and what was typed back,
//! interleaved as a terminal shows them. [`Transcript`] echoes each answer
//! into the same buffer the prompt writes to, exactly as a terminal echoes a
//! keypress. Without that, a pin could show `(unrecognized; pick one of
//! k/r/e/s/a)` without showing which key provoked it.
//!
//! Accepting a deliberate change to a prompt:
//!
//! ```text
//! RDC_UPDATE_PINS=1 cargo test --lib prompt_pin
//! ```
//!
//! then read the diff before committing it.

use std::path::Path;

/// Environment variable that rewrites every pin instead of asserting.
const UPDATE: &str = "RDC_UPDATE_PINS";

/// Replaces the process-specific tempdir prefix with a stable placeholder.
///
/// `tempfile::tempdir()` mints a fresh random path every run and that path
/// lands verbatim in a prompt's connector line, so a pin containing it could
/// never match twice, let alone survive being committed.
pub(crate) fn redact_tempdir(actual: &str, tmp: &Path) -> String {
    actual.replace(&tmp.display().to_string(), "TMPDIR")
}

/// Renders an `inquire` prompt in the shape this directory pins it: the
/// question, then each option on its own line, indented.
pub(crate) fn inquire_shape(question: &str, options: &[String]) -> String {
    let mut s = String::from(question);
    for o in options {
        s.push_str("\n  ");
        s.push_str(o);
    }
    s
}

/// Compares `actual` against `testdata/prompt_pins/<name>.txt`.
///
/// Writes the file and fails on first run — a new pin is reviewed as a diff
/// against nothing, then re-run to confirm it reproduces. With [`UPDATE`] set
/// it rewrites the file and passes, which is how a deliberate prompt change
/// is accepted.
pub(crate) fn pin(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/prompt_pins")
        .join(format!("{name}.txt"));
    let updating = std::env::var_os(UPDATE).is_some();
    if updating || !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        if updating {
            return;
        }
        panic!("wrote a new pin at {}; re-run to verify it", path.display());
    }
    let expected = std::fs::read_to_string(&path).unwrap();
    pretty_assertions::assert_eq!(expected, actual, "prompt bytes moved: {}", name);
}

/// A [`crate::cli::stdin_coord::PromptRoute`] that answers every prompt with
/// the same canned string, so a destructive gate can be driven to completion
/// with no terminal attached.
pub(crate) struct CannedRoute {
    answer: &'static str,
    sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl CannedRoute {
    /// `sink` is the same buffer the gate's `Log` writes to, so the answer
    /// lands in the record next to the question — see [`Transcript`].
    pub(crate) fn echoing(
        answer: &'static str,
        sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    ) -> Self {
        Self { answer, sink }
    }
}

impl crate::cli::stdin_coord::PromptRoute for CannedRoute {
    fn ask(&self, _prompt: &crate::cli::stdin_coord::Prompt) -> Option<String> {
        let mut g = self.sink.lock().unwrap();
        g.extend_from_slice(self.answer.as_bytes());
        g.push(b'\n');
        drop(g);
        Some(self.answer.to_string())
    }
}

/// A terminal transcript for the prompts that take a reader and a writer.
///
/// `input` serves the canned keystrokes and echoes each one into the shared
/// buffer as it is read; `output` is what the prompt writes to. `text` is the
/// two interleaved, which is what gets pinned.
pub(crate) struct Transcript {
    buf: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
}

impl Transcript {
    pub(crate) fn new() -> Self {
        Self { buf: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())) }
    }

    /// `keys` is what the user types, newline-separated, e.g. `"z\ns\n"`.
    pub(crate) fn input(&self, keys: &str) -> TranscriptInput {
        TranscriptInput {
            inner: std::io::Cursor::new(keys.as_bytes().to_vec()),
            sink: self.buf.clone(),
        }
    }

    pub(crate) fn output(&self) -> TranscriptOutput {
        TranscriptOutput { sink: self.buf.clone() }
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf.borrow()).into_owned()
    }
}

pub(crate) struct TranscriptInput {
    inner: std::io::Cursor<Vec<u8>>,
    sink: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
}

impl std::io::Read for TranscriptInput {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.inner, out)
    }
}

impl std::io::BufRead for TranscriptInput {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        std::io::BufRead::fill_buf(&mut self.inner)
    }

    fn consume(&mut self, n: usize) {
        std::io::BufRead::consume(&mut self.inner, n)
    }

    /// Overridden to echo. Every prompt reads its answer through this one
    /// method, so this is the only place a keystroke can enter the record.
    fn read_line(&mut self, out: &mut String) -> std::io::Result<usize> {
        let before = out.len();
        let n = std::io::BufRead::read_line(&mut self.inner, out)?;
        if n > 0 {
            let typed = &out[before..];
            let mut sink = self.sink.borrow_mut();
            sink.extend_from_slice(typed.as_bytes());
            if !typed.ends_with('\n') {
                sink.push(b'\n');
            }
        }
        Ok(n)
    }
}

pub(crate) struct TranscriptOutput {
    sink: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
}

impl std::io::Write for TranscriptOutput {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.sink.borrow_mut().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A `Log` over `sink` with a fixed clock, so pinned event lines do not move
/// with the wall clock. The fixed-clock path formats in UTC, so the pin is
/// the same on every machine.
pub(crate) fn pinned_log(
    sink: Box<dyn std::io::Write + Send>,
) -> std::sync::Arc<crate::log::Log> {
    crate::log::Log::for_test_with_time(
        crate::cli::resolve::ColorMode::Plain,
        sink,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(9 * 3600 + 41 * 60 + 5),
    )
}
