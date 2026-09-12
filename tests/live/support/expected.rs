use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Normalized, run-agnostic snapshot of the facts a scenario pins.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CapturedState {
    /// kind -> sorted lockfile slugs, with the run-id prefix stripped.
    #[serde(default)]
    pub lockfile_keys: BTreeMap<String, Vec<String>>,
    /// label -> portable ref value found on disk (run-id stripped).
    #[serde(default)]
    pub refs: BTreeMap<String, String>,
}

/// Which of [`load_or_compare`]'s two jobs the caller is asking for.
///
/// The point of the type is WHERE the decision is made. It is taken once, by
/// the test function that knows which backend it is running against, and
/// carried into the comparison as a value — instead of being re-read from the
/// process environment at the moment of the write, seconds later, on a
/// different thread.
///
/// That difference is not theoretical. `load_or_compare` used to call
/// [`capture_mode`] itself, and `tests::round_trips_via_toml` used to set
/// `RDC_LIVE_CAPTURE` with `std::env::set_var`, which is process-wide — the
/// same process that runs every fake-backed scenario in this binary, on
/// libtest's default thread pool. A fake wrapper's `assert!(!capture_mode())`
/// had already passed by then, so the window between that assert and the
/// write was all it took. DEMONSTRATED, not deduced: widen the unit test's
/// window to twelve seconds, add a bogus row to
/// `testdata/live/expected/server_truth.toml`, and run it alongside
/// `fake_trailing_whitespace_handling_is_unchanged` — the fake-backed twin
/// deleted the row, wrote the fake's own answers over a golden captured from
/// a real organization, and reported `ok`. The "CAPTURED golden" notice goes
/// to stderr, which cargo swallows without `--nocapture`.
///
/// With the decision threaded instead, a fake-backed run cannot capture no
/// matter what any other thread does to the environment: its wrapper hands
/// down [`Golden::Compare`], and nothing downstream consults the env at all.
/// `tests::no_fake_wrapper_can_ask_for_a_capture` pins the wrappers' half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Golden {
    /// Assert `actual` equals the committed golden.
    Compare,
    /// Overwrite the golden with `actual`. Only a LIVE run may ask for this:
    /// a golden is evidence about a real Rossum organization, and a capture
    /// from any other backend replaces that evidence with an opinion.
    Capture,
}

impl Golden {
    /// The live wrappers' entry point: read `RDC_LIVE_CAPTURE` ONCE, at the
    /// top of the test, and carry the answer down. The fake wrappers never
    /// call this — they pass [`Golden::Compare`] literally.
    #[allow(dead_code)]
    pub fn from_env() -> Golden {
        if capture_mode() { Golden::Capture } else { Golden::Compare }
    }
}

/// Returns true when the harness should WRITE golden files instead of
/// asserting against them (first capture / intentional re-baseline).
///
/// Read by [`Golden::from_env`] and by each fake wrapper's opening assert —
/// never inside [`load_or_compare`], which is what keeps the capture decision
/// from moving after a wrapper has already vetted it.
#[allow(dead_code)]
pub fn capture_mode() -> bool {
    std::env::var("RDC_LIVE_CAPTURE").map(|v| v == "1").unwrap_or(false)
}

/// Compare `actual` to the golden file at `path`, or — with
/// [`Golden::Capture`] — write `actual` and pass (the maintainer reviews the
/// diff before committing).
#[allow(dead_code)]
pub fn load_or_compare(path: &Path, actual: &CapturedState, mode: Golden) -> Result<()> {
    if mode == Golden::Capture {
        let toml = toml::to_string_pretty(actual).context("serializing captured state")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(path, toml).with_context(|| format!("writing golden {}", path.display()))?;
        eprintln!("CAPTURED golden state -> {} (review before committing)", path.display());
        return Ok(());
    }
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "missing golden {}. Run once with RDC_LIVE_CAPTURE=1 to create it, then review.",
            path.display()
        )
    })?;
    let expected: CapturedState = toml::from_str(&raw)?;
    if &expected != actual {
        bail!(
            "captured state mismatch for {}\nexpected: {:#?}\nactual:   {:#?}",
            path.display(),
            expected,
            actual
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Both modes over one golden file. Touches no environment variable at
    /// all: [`Golden`] is passed in, so this test can no longer reach into
    /// the process environment every other test in this binary shares — see
    /// [`Golden`]'s doc comment for what that reach cost.
    #[test]
    fn round_trips_via_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("expected/x.toml");
        let mut st = CapturedState::default();
        st.lockfile_keys.insert("queues".into(), vec!["a".into(), "b".into()]);
        st.refs.insert("q.schema".into(), "rdc://schemas/a".into());
        // capture
        load_or_compare(&path, &st, Golden::Capture).unwrap();
        // compare equal
        load_or_compare(&path, &st, Golden::Compare).unwrap();
        // compare unequal
        let mut other = CapturedState::default();
        other.lockfile_keys.insert("queues".into(), vec!["a".into()]);
        assert!(load_or_compare(&path, &other, Golden::Compare).is_err());
    }

    /// A `Golden::Compare` run must never write, even when the golden is
    /// missing — the one case where a capture-by-accident would look like a
    /// first capture rather than like damage.
    #[test]
    fn a_missing_golden_is_an_error_not_a_capture() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("expected/never-written.toml");
        assert!(load_or_compare(&path, &CapturedState::default(), Golden::Compare).is_err());
        assert!(!path.exists(), "Compare must not create the golden it could not read");
    }

    /// The wrappers' half of the guarantee: no `fake_*` test function may ask
    /// for a capture, whether by naming [`Golden::Capture`] or by deferring
    /// to the environment through [`Golden::from_env`]. Both belong to the
    /// `live_*` twins.
    ///
    /// Checks the SOURCE, because that is where the property lives — and for
    /// the same weak-but-real thing
    /// `fake::quirks::every_live_citation_actually_proves_it` checks: it
    /// cannot tell whether a wrapper is otherwise correct, only that this one
    /// mistake is not present. Chunks each file at `async fn ` boundaries; a
    /// shared scenario body takes the mode as a parameter and so names
    /// neither symbol.
    ///
    /// A function's own doc comment sits BEFORE its `async fn`, so it falls in
    /// the preceding chunk: what each chunk holds is one body plus the next
    /// function's doc comment. That only ever over-reports — a doc comment
    /// that discussed `Golden::from_env` right above a `fake_*` wrapper would
    /// fail this guard — which is the safe direction, and worth knowing before
    /// writing such a sentence.
    #[test]
    fn no_fake_wrapper_can_ask_for_a_capture() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live/scenarios");
        let mut fake_wrappers = 0;
        let mut live_wrappers_reading_the_env = 0;
        for entry in std::fs::read_dir(&root).expect("scenarios dir").flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read scenario");
            let file = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            for chunk in src.split("async fn ").skip(1) {
                let name = chunk.split(['(', '<']).next().unwrap_or("").trim().to_string();
                if name.starts_with("fake_") {
                    fake_wrappers += 1;
                    for banned in ["Golden::Capture", "Golden::from_env"] {
                        assert!(
                            !chunk.contains(banned),
                            "{file}::{name} names {banned}: a fake-backed test must pass \
                             Golden::Compare, so that no environment variable and no other \
                             thread can turn it into a capture"
                        );
                    }
                } else if name.starts_with("live_") && chunk.contains("Golden::from_env") {
                    live_wrappers_reading_the_env += 1;
                }
            }
        }
        assert!(fake_wrappers > 0, "no fake_* wrapper found — this guard would check nothing");
        assert!(
            live_wrappers_reading_the_env > 0,
            "no live_* wrapper reads Golden::from_env — the capture path is gone, and this \
             guard is watching for a mistake nobody can make any more"
        );
    }
}
