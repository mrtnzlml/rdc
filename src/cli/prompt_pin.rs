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
//! A pin holds only the bytes rdc writes. Where a question is followed
//! immediately by more output, the newline the terminal echoes when the user
//! presses Enter is absent — the answer arrives from a `Cursor`, which echoes
//! nothing.
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
pub(crate) struct CannedRoute(pub &'static str);

impl crate::cli::stdin_coord::PromptRoute for CannedRoute {
    fn ask(&self, _prompt: &crate::cli::stdin_coord::Prompt) -> Option<String> {
        Some(self.0.to_string())
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
