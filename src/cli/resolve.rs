//! Interactive conflict resolver (spec §8.3).
//!
//! When a three-way pull detects both local and remote diverged from base,
//! the resolver presents an inline prompt:
//!
//! ```text
//! [1/N]  hooks/validator-invoices.json
//!
//! Local has changes:
//!   <unified diff snippet>
//!
//! Remote has changes:
//!   <unified diff snippet>
//!
//! [k] keep local   [r] keep remote   [e] edit   [s] skip   [a] abort >
//! ```
//!
//! `[k]` keeps local, no write. `[r]` writes remote. `[e]` opens `$EDITOR`
//! on a temp file (with conflict markers) and uses the saved bytes. `[s]`
//! falls through to the original shadow-file behavior (writes
//! `<file>.remote`, keeps local). `[a]` bubbles a `PullAborted` error so
//! the caller stops without saving the lockfile.
//!
//! Activation: only on TTY stdin AND when `--yes` is not set. Otherwise
//! callers fall through to shadow-file (legacy behavior, CI-safe).

use anyhow::{Context, Result};
use similar::TextDiff;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

/// Outcome of presenting a single conflict to the user.
#[derive(Debug)]
pub enum Resolution {
    /// Keep the local file as-is. No write. Lockfile records local hash.
    KeepLocal,
    /// Overwrite local with the remote bytes. Lockfile records remote hash.
    KeepRemote,
    /// Use these (user-edited) bytes. Lockfile records hash of these bytes.
    Edit(Vec<u8>),
    /// Treat this as the legacy shadow-file behavior — write
    /// `<file>.remote`, keep local. Lockfile records local hash.
    Skip,
    /// Abort the entire pull. Caller stops without saving the lockfile.
    Abort,
}

/// Returns true if interactive resolution is appropriate for this process.
/// False when stdin is not a TTY, or when the user passed `--yes`.
pub fn is_interactive(yes_flag: bool) -> bool {
    !yes_flag && std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Render a unified diff (3 lines of context) suitable for inline display.
/// Returns the diff as a string; an empty string means the two slices are
/// byte-identical.
pub fn unified_diff(label_a: &str, a: &[u8], label_b: &str, b: &[u8]) -> String {
    let a_str = String::from_utf8_lossy(a);
    let b_str = String::from_utf8_lossy(b);
    let diff = TextDiff::from_lines(a_str.as_ref(), b_str.as_ref());
    let mut out = String::new();
    out.push_str(&format!("--- {label_a}\n"));
    out.push_str(&format!("+++ {label_b}\n"));
    let mut any = false;
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        any = true;
        out.push_str(&format!("{hunk}"));
    }
    if !any {
        return String::new();
    }
    out
}

/// Read base bytes from `local_path` if it exists; used for the
/// "local has changes" / "remote has changes" header in the prompt.
fn read_local(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

/// Prompt the user to resolve a conflict on `local_path` between the
/// current local bytes and proposed `remote_bytes`. Caller passes
/// `(index, total)` for the `[N/M]` header.
///
/// Reads from stdin via `BufRead` so tests can supply a `Cursor`. The
/// production caller wraps `std::io::stdin().lock()`.
pub fn prompt_resolve<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    remote_bytes: &[u8],
) -> Result<Resolution> {
    let local_bytes = read_local(local_path)?;

    writeln!(output, "")?;
    writeln!(output, "[{index}/{total}]  {} — conflict", local_path.display())?;
    writeln!(output, "")?;

    let diff = unified_diff("local", &local_bytes, "remote", remote_bytes);
    if diff.is_empty() {
        // Defensive: caller already determined a conflict, but if local
        // and remote are byte-identical we just keep local.
        return Ok(Resolution::KeepLocal);
    }
    write!(output, "{diff}")?;
    writeln!(output, "")?;

    loop {
        write!(output, "[k]eep local  [r]emote  [e]dit  [s]kip (shadow file)  [a]bort > ")?;
        output.flush().ok();
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            // EOF — treat as skip (preserve legacy behavior).
            return Ok(Resolution::Skip);
        }
        match line.trim().chars().next() {
            Some('k') | Some('K') => return Ok(Resolution::KeepLocal),
            Some('r') | Some('R') => return Ok(Resolution::KeepRemote),
            Some('s') | Some('S') => return Ok(Resolution::Skip),
            Some('a') | Some('A') => return Ok(Resolution::Abort),
            Some('e') | Some('E') => {
                let edited = run_editor_with_markers(&local_bytes, remote_bytes)?;
                return Ok(Resolution::Edit(edited));
            }
            _ => {
                writeln!(output, "  (unrecognized — pick one of k/r/e/s/a)")?;
                continue;
            }
        }
    }
}

/// Open `$EDITOR` (or `vi`) on a temp file pre-populated with git-style
/// conflict markers. After the editor exits, return the file's bytes.
fn run_editor_with_markers(local: &[u8], remote: &[u8]) -> Result<Vec<u8>> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

    let dir = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("rdc-conflict-{stamp}.tmp"));

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"<<<<<<< local\n");
    buf.extend_from_slice(local);
    if !local.ends_with(b"\n") {
        buf.push(b'\n');
    }
    buf.extend_from_slice(b"=======\n");
    buf.extend_from_slice(remote);
    if !remote.ends_with(b"\n") {
        buf.push(b'\n');
    }
    buf.extend_from_slice(b">>>>>>> remote\n");

    std::fs::write(&path, &buf)
        .with_context(|| format!("writing temp conflict file {}", path.display()))?;

    // Spawn the editor; inherit stdio so the user actually sees it.
    let status = Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("spawning editor '{editor}'"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        anyhow::bail!("editor '{editor}' exited with non-zero status");
    }

    let edited = std::fs::read(&path)
        .with_context(|| format!("reading edited conflict file {}", path.display()))?;
    let _ = std::fs::remove_file(&path);
    Ok(edited)
}

/// Resolve a single sub-file within a combined-hash entity (hook
/// `.json`/`.py`, schema `schema.json`/formulas/`<id>.py`). M33 / spec §8.3.
///
/// The caller passes the in-memory bytes for both sides. Behavior:
///
/// - `local_bytes == remote_bytes` → no-op (no prompt, no write); returns
///   `local_bytes`.
/// - `interactive == false` → legacy shadow-file: writes
///   `<local_path>.remote`, keeps local on disk, returns `local_bytes`.
/// - `interactive == true && bytes differ` → prompt the user via
///   [`prompt_resolve`] with `[label_index/label_total]`. On Skip / Keep
///   semantics match [`apply_pull_action`]. On Abort: propagate
///   [`PullAborted`] so the caller bubbles up.
///
/// Returns the bytes that are now on disk for `local_path`. The caller
/// uses these to compute the entity's combined hash.
pub fn resolve_combined_file(
    label_index: usize,
    label_total: usize,
    local_path: &Path,
    local_bytes: &[u8],
    remote_bytes: &[u8],
    interactive: bool,
) -> Result<Vec<u8>> {
    use crate::snapshot::writer::write_atomic;

    if local_bytes == remote_bytes {
        return Ok(local_bytes.to_vec());
    }

    if !interactive {
        let conflict_path = shadow_path_for(local_path);
        write_atomic(&conflict_path, remote_bytes)?;
        eprintln!(
            "warning: {} conflict — local preserved, remote at {}",
            local_path.display(),
            conflict_path.display()
        );
        return Ok(local_bytes.to_vec());
    }

    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    let resolution = prompt_resolve(
        stdin.lock(),
        stderr.lock(),
        label_index,
        label_total,
        local_path,
        remote_bytes,
    )?;
    match resolution {
        Resolution::KeepLocal => Ok(local_bytes.to_vec()),
        Resolution::KeepRemote => {
            write_atomic(local_path, remote_bytes)?;
            Ok(remote_bytes.to_vec())
        }
        Resolution::Edit(edited) => {
            write_atomic(local_path, &edited)?;
            Ok(edited)
        }
        Resolution::Skip => {
            let conflict_path = shadow_path_for(local_path);
            write_atomic(&conflict_path, remote_bytes)?;
            eprintln!(
                "warning: {} conflict — local preserved, remote at {}",
                local_path.display(),
                conflict_path.display()
            );
            Ok(local_bytes.to_vec())
        }
        Resolution::Abort => Err(anyhow::Error::new(PullAborted)),
    }
}

/// Compute the `<file>.remote` shadow path for a given local file.
fn shadow_path_for(local_path: &Path) -> std::path::PathBuf {
    let mut conflict_path = local_path.to_path_buf();
    let new_name = match conflict_path.file_name().and_then(|s| s.to_str()) {
        Some(name) => format!("{name}.remote"),
        None => "remote".to_string(),
    };
    conflict_path.set_file_name(new_name);
    conflict_path
}

/// Sentinel error type signaling the user picked `[a]bort`. The pull
/// runner downcasts to this and skips lockfile.save().
#[derive(Debug, thiserror::Error)]
#[error("pull aborted by user at conflict resolver")]
pub struct PullAborted;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn unified_diff_empty_when_identical() {
        assert!(unified_diff("a", b"hello\n", "b", b"hello\n").is_empty());
    }

    #[test]
    fn unified_diff_renders_changed_lines() {
        let d = unified_diff("local", b"a\nb\nc\n", "remote", b"a\nB\nc\n");
        assert!(d.contains("--- local"), "got: {d}");
        assert!(d.contains("+++ remote"), "got: {d}");
        assert!(d.contains("-b"), "got: {d}");
        assert!(d.contains("+B"), "got: {d}");
    }

    #[test]
    fn prompt_keep_local_returns_keep_local() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();

        let input = Cursor::new(b"k\n");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"remote\n").unwrap();
        assert!(matches!(r, Resolution::KeepLocal));
        let s = String::from_utf8(output).unwrap();
        assert!(s.contains("[1/1]"), "output: {s}");
    }

    #[test]
    fn prompt_keep_remote_returns_keep_remote() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();

        let input = Cursor::new(b"r\n");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"remote\n").unwrap();
        assert!(matches!(r, Resolution::KeepRemote));
    }

    #[test]
    fn prompt_skip_returns_skip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();

        let input = Cursor::new(b"s\n");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 2, 5, &path, b"remote\n").unwrap();
        assert!(matches!(r, Resolution::Skip));
    }

    #[test]
    fn prompt_abort_returns_abort() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();

        let input = Cursor::new(b"a\n");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"remote\n").unwrap();
        assert!(matches!(r, Resolution::Abort));
    }

    #[test]
    fn prompt_unrecognized_re_prompts_then_accepts() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();

        let input = Cursor::new(b"q\nx\n\nk\n");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"remote\n").unwrap();
        assert!(matches!(r, Resolution::KeepLocal));
        let s = String::from_utf8(output).unwrap();
        assert!(s.contains("unrecognized"), "output: {s}");
    }

    #[test]
    fn prompt_eof_falls_back_to_skip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();

        // Empty input — first read_line returns 0 (EOF).
        let input = Cursor::new(b"");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"remote\n").unwrap();
        assert!(matches!(r, Resolution::Skip));
    }

    #[test]
    fn prompt_skips_when_local_equals_remote() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"same\n").unwrap();

        // No input read — function short-circuits because local == remote.
        let input = Cursor::new(b"");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"same\n").unwrap();
        assert!(matches!(r, Resolution::KeepLocal));
    }

    #[test]
    fn is_interactive_false_under_cargo_test() {
        // Cargo test stdin is not a TTY, so this is always false here.
        assert!(!is_interactive(false));
        // --yes always returns false regardless of TTY.
        assert!(!is_interactive(true));
    }

    #[test]
    fn resolve_combined_file_noop_when_equal() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.py");
        std::fs::write(&path, b"same\n").unwrap();
        let out = resolve_combined_file(1, 2, &path, b"same\n", b"same\n", true).unwrap();
        assert_eq!(out, b"same\n");
        // No shadow file written.
        assert!(!dir.path().join("a.py.remote").exists());
    }

    #[test]
    fn resolve_combined_file_writes_shadow_when_non_interactive() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.py");
        std::fs::write(&path, b"local\n").unwrap();
        let out = resolve_combined_file(1, 1, &path, b"local\n", b"remote\n", false).unwrap();
        assert_eq!(out, b"local\n");
        assert_eq!(std::fs::read(dir.path().join("a.py.remote")).unwrap(), b"remote\n");
        // Local file untouched.
        assert_eq!(std::fs::read(&path).unwrap(), b"local\n");
    }

    #[test]
    fn shadow_path_inserts_remote_suffix() {
        let p = std::path::PathBuf::from("/tmp/x/y.json");
        assert_eq!(shadow_path_for(&p), std::path::PathBuf::from("/tmp/x/y.json.remote"));
    }

    #[test]
    fn shadow_path_for_py_extension() {
        let p = std::path::PathBuf::from("/tmp/formulas/123.py");
        assert_eq!(shadow_path_for(&p), std::path::PathBuf::from("/tmp/formulas/123.py.remote"));
    }
}
