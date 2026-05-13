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
use std::fmt::Write as FmtWrite;
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
    write!(out, "--- {label_a}\n").expect("writing to String never fails");
    write!(out, "+++ {label_b}\n").expect("writing to String never fails");
    let mut any = false;
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        any = true;
        write!(out, "{hunk}").expect("writing to String never fails");
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

/// Top-level entry point. Auto-detects color mode from environment and TTY.
/// Production callers use this; tests use `prompt_resolve_with_color` to
/// pin the mode.
pub fn prompt_resolve<R: BufRead, W: Write>(
    input: R,
    output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    remote_bytes: &[u8],
) -> Result<Resolution> {
    let mode = detect_color_mode(false);
    prompt_resolve_with_color(input, output, index, total, local_path, remote_bytes, mode)
}

/// Color-aware core. Tests pin the mode here; production goes through
/// `prompt_resolve` which auto-detects.
pub fn prompt_resolve_with_color<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    remote_bytes: &[u8],
    mode: ColorMode,
) -> Result<Resolution> {
    let local_bytes = read_local(local_path)?;

    // Strip noise fields before diff display so the user only sees real
    // changes. modified_at server-churn must not appear in the resolver.
    let local_canonical = crate::snapshot::noise::canonicalize_for_hash(&local_bytes);
    let remote_canonical = crate::snapshot::noise::canonicalize_for_hash(remote_bytes);

    if local_canonical == remote_canonical {
        return Ok(Resolution::KeepLocal);
    }

    writeln!(output)?;
    let header = format!("[{index}/{total}]  {} — conflict", local_path.display());
    writeln!(output, "{}", colorize_header(&header, mode))?;
    writeln!(output)?;

    let diff = unified_diff("local", &local_canonical, "remote", &remote_canonical);
    if diff.is_empty() {
        return Ok(Resolution::KeepLocal);
    }
    for line in diff.lines() {
        writeln!(output, "{}", colorize_diff_line(line, mode))?;
    }
    writeln!(output)?;

    loop {
        let prompt_text = "[k]eep local  [r]emote  [e]dit  [s]kip (shadow file)  [a]bort > ";
        write!(output, "{}", colorize_prompt(prompt_text, mode))?;
        output.flush().ok();
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
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
/// `.json`/`.py`, schema `schema.json`/formulas/`<id>.py`). Spec §8.3.
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

/// Outcome of a push-drift prompt (spec §7.3 step 5). Different from a
/// pull-side [`Resolution`] because the user's choices have different
/// consequences on push:
///
/// - `Patch { payload_override: None }`: force-push the caller's prepared
///   payload, overwriting whatever drift exists on the remote. (`[k]`)
/// - `Patch { payload_override: Some(bytes) }`: same, but PATCH `bytes`
///   instead of the prepared payload (user picked `[e]dit`). The caller
///   re-deserializes the bytes to its typed model.
/// - `Adopt`: abandon the local edit. Write `remote_bytes` to the local
///   file and record `remote_hash` in the lockfile. No PATCH. (`[r]`)
/// - `Skip`: do nothing — leave local and lockfile alone. Warn the user.
///   This is the fallback when stdin isn't a TTY or `--yes` is set. (`[s]`)
///
/// `[a]bort` propagates as a [`PullAborted`] error so the push runner
/// can stop and skip lockfile.save().
#[derive(Debug)]
pub enum PushDriftOutcome {
    /// Proceed with PATCH. `payload_override`: when `Some`, the user
    /// edited the proposed bytes; the caller should use these instead
    /// of its prepared payload.
    Patch { payload_override: Option<Vec<u8>> },
    /// Abandon local edit, take remote into local + lockfile.
    Adopt,
    /// Skip this object — current behavior, leaves both alone.
    Skip,
}

/// Resolve a push-side drift conflict (spec §7.3 step 5). Caller passes
/// the on-disk local path, the bytes the user wants to push, and the
/// (overlay-stripped) bytes currently on the server.
///
/// When `interactive == false` (CI / non-TTY / `--yes`), returns
/// `PushDriftOutcome::Skip` to preserve legacy behavior.
///
/// On `[k]eep local`: returns `Patch { payload_override: None }` —
/// caller PATCHes its prepared payload (force-push).
/// On `[r]emote`: returns `Adopt` — caller writes remote to local +
/// lockfile, no PATCH.
/// On `[e]dit`: opens `$EDITOR`, returns `Patch { payload_override:
/// Some(edited_bytes) }`.
/// On `[s]kip`: returns `Skip`.
/// On `[a]bort`: returns a `PullAborted` error.
pub fn resolve_push_drift(
    interactive: bool,
    local_path: &Path,
    remote_bytes: &[u8],
) -> Result<PushDriftOutcome> {
    if !interactive {
        return Ok(PushDriftOutcome::Skip);
    }

    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    let resolution = prompt_resolve(
        stdin.lock(),
        stderr.lock(),
        1,
        1,
        local_path,
        remote_bytes,
    )?;
    match resolution {
        Resolution::KeepLocal => Ok(PushDriftOutcome::Patch { payload_override: None }),
        Resolution::KeepRemote => Ok(PushDriftOutcome::Adopt),
        Resolution::Edit(edited) => Ok(PushDriftOutcome::Patch { payload_override: Some(edited) }),
        Resolution::Skip => Ok(PushDriftOutcome::Skip),
        Resolution::Abort => Err(anyhow::Error::new(PullAborted)),
    }
}

fn sort_users_for_picker(users: &[crate::model::User]) -> Vec<&crate::model::User> {
    let mut v: Vec<&crate::model::User> = users.iter().collect();
    v.sort_by_key(|u| {
        if u.is_system_user() { 0u8 }
        else if u.is_admin() { 1 }
        else { 2 }
    });
    v
}

/// Render the token_owner picker prompt as a string. Pure function for
/// testability; the interactive variant `prompt_token_owner` wraps this
/// with stdin/stdout.
pub fn render_token_owner_picker(
    slug: &str,
    tgt_env: &str,
    users: &[crate::model::User],
    self_user_id: Option<u64>,
) -> String {
    let sorted = sort_users_for_picker(users);
    let mut out = String::new();
    write!(out, "Pick the token_owner for store extension '{slug}' on {tgt_env}\n").expect("writing to String never fails");
    write!(out, "(used as the API service account for the extension's calls; usually a system user):\n\n").expect("writing to String never fails");
    for (i, u) in sorted.iter().enumerate() {
        let mut tags = Vec::new();
        if u.is_admin() { tags.push("admin"); }
        if u.is_active { tags.push("active"); }
        if Some(u.id) == self_user_id { tags.push("you"); }
        let tags = tags.join(", ");
        let display = if u.first_name.is_empty() && u.last_name.is_empty() {
            u.username.clone()
        } else {
            format!("{} {}", u.first_name, u.last_name).trim().to_string()
        };
        write!(out, "  [{}] {display}   {tags}\n", i + 1).expect("writing to String never fails");
        write!(out, "      {}\n", u.url).expect("writing to String never fails");
    }
    write!(out, "  [a] abort the deploy\n\n").expect("writing to String never fails");
    write!(out, "[1] > ").expect("writing to String never fails");
    out
}

/// Prompt interactively. Returns `Some((picked_user_url, apply_to_all))`
/// or `None` if the user aborted. Non-TTY callers must skip this and
/// check the overlay state up-front.
pub fn prompt_token_owner(
    slug: &str,
    tgt_env: &str,
    users: &[crate::model::User],
    self_user_id: Option<u64>,
) -> anyhow::Result<Option<(String, bool)>> {
    use std::io::{self, BufRead, Write};
    let rendered = render_token_owner_picker(slug, tgt_env, users, self_user_id);
    print!("{rendered}");
    io::stdout().flush().ok();
    let stdin = io::stdin();
    let line = stdin.lock().lines().next().ok_or_else(|| anyhow::anyhow!("stdin closed"))??;
    let pick = line.trim();
    if pick == "a" { return Ok(None); }
    let n: usize = if pick.is_empty() {
        1
    } else {
        pick.parse().map_err(|_| anyhow::anyhow!("expected a number or 'a', got '{pick}'"))?
    };
    let sorted = sort_users_for_picker(users);
    let chosen = sorted.get(n - 1).ok_or_else(|| anyhow::anyhow!("'{n}' is out of range"))?;
    print!("\nApply this choice to all remaining store extensions in this deploy? [y/N] ");
    io::stdout().flush().ok();
    let line2 = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
    let apply_all = matches!(line2.trim().to_lowercase().as_str(), "y" | "yes");
    Ok(Some((chosen.url.clone(), apply_all)))
}

/// Sentinel error type signaling the user picked `[a]bort` at any
/// resolver prompt (pull or push). The pull / push runner downcasts to
/// this and skips lockfile.save().
#[derive(Debug, thiserror::Error)]
#[error("aborted by user at conflict resolver")]
pub struct PullAborted;

/// Whether to emit ANSI color codes in resolver output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Plain,
    Color,
}

/// Process-wide override for the `--no-color` CLI flag. Set once at
/// `rdc` startup by `cli::run`; read by `detect_color_mode`. Using an
/// atomic instead of threading the flag through every PullCtx /
/// PushDriftOutcome / Apply call site — the flag is set exactly once
/// and never changes during a run.
static NO_COLOR_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record the `--no-color` flag value from the CLI parser.
pub fn set_no_color_flag(no_color: bool) {
    NO_COLOR_FLAG.store(no_color, std::sync::atomic::Ordering::Relaxed);
}

/// Decide the color mode at runtime. `--no-color` flag has highest priority,
/// then NO_COLOR env var, then stderr TTY detection.
pub fn detect_color_mode(no_color_flag: bool) -> ColorMode {
    decide_color_mode(
        no_color_flag || NO_COLOR_FLAG.load(std::sync::atomic::Ordering::Relaxed),
        std::env::var_os("NO_COLOR").is_some(),
        std::io::stderr().is_terminal(),
    )
}

/// Pure form for testing: returns the color mode given the three inputs
/// directly. The wrapping `detect_color_mode` plumbs in the live env +
/// TTY readings.
fn decide_color_mode(no_color: bool, no_color_env: bool, is_tty: bool) -> ColorMode {
    if no_color || no_color_env {
        return ColorMode::Plain;
    }
    if is_tty {
        ColorMode::Color
    } else {
        ColorMode::Plain
    }
}

/// Apply color to a single line of unified-diff output. Returns `line`
/// unchanged in [`ColorMode::Plain`].
pub fn colorize_diff_line(line: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return line.to_string();
    }
    let prefix = if line.starts_with("--- ") {
        "\x1b[91m" // bright red
    } else if line.starts_with("+++ ") {
        "\x1b[92m" // bright green
    } else if line.starts_with("@@") {
        "\x1b[36m" // cyan
    } else if line.starts_with('-') {
        "\x1b[91m" // bright red
    } else if line.starts_with('+') {
        "\x1b[92m" // bright green
    } else {
        return line.to_string();
    };
    format!("{prefix}{line}\x1b[0m")
}

/// Colorize the conflict header line. Bold yellow.
pub fn colorize_header(text: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return text.to_string();
    }
    format!("\x1b[1;93m{text}\x1b[0m")
}

/// Colorize the action-letter prompt line. Bracketed single-letter tokens
/// like `[k]` are wrapped in bold cyan; the rest of the prompt is unchanged.
pub fn colorize_prompt(text: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 64);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            if let Some(&letter) = chars.peek() {
                if letter.is_ascii_alphabetic() {
                    chars.next(); // consume letter
                    if matches!(chars.peek(), Some(']')) {
                        chars.next(); // consume ]
                        out.push_str("[\x1b[1;96m");
                        out.push(letter);
                        out.push_str("\x1b[0m]");
                        continue;
                    } else {
                        // Not a single-letter bracketed token — emit as-is.
                        out.push('[');
                        out.push(letter);
                        continue;
                    }
                }
            }
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

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

    #[test]
    fn resolve_push_drift_non_interactive_returns_skip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();
        let r = resolve_push_drift(false, &path, b"remote\n").unwrap();
        assert!(matches!(r, PushDriftOutcome::Skip));
    }

    #[test]
    fn prompt_short_circuits_when_only_noise_differs() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{\"name\":\"x\",\"modified_at\":\"t1\"}").unwrap();

        // Empty input — function must not block on read_line.
        let input = Cursor::new(b"");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(
            input,
            &mut output,
            1,
            1,
            &path,
            b"{\"name\":\"x\",\"modified_at\":\"t2\"}",
        )
        .unwrap();
        assert!(matches!(r, Resolution::KeepLocal));
        // No prompt was rendered (short-circuit).
        let s = String::from_utf8(output).unwrap();
        assert!(!s.contains("[k]eep"), "should not have prompted: {s}");
    }

    #[test]
    fn colorize_plain_mode_returns_unchanged() {
        let line = "-  \"name\": \"old\"";
        assert_eq!(colorize_diff_line(line, ColorMode::Plain), line.to_string());
    }

    #[test]
    fn colorize_color_mode_renders_minus_red() {
        let line = "-  \"name\": \"old\"";
        let out = colorize_diff_line(line, ColorMode::Color);
        // Bright-red SGR = \x1b[91m, reset = \x1b[0m.
        assert!(out.contains("\x1b[91m"), "expected bright red prefix in: {out:?}");
        assert!(out.ends_with("\x1b[0m"), "expected reset suffix in: {out:?}");
    }

    #[test]
    fn colorize_color_mode_renders_plus_green() {
        let line = "+  \"name\": \"new\"";
        let out = colorize_diff_line(line, ColorMode::Color);
        assert!(out.contains("\x1b[92m"), "expected bright green prefix in: {out:?}");
    }

    #[test]
    fn colorize_color_mode_leaves_context_lines_alone() {
        let line = "   \"unchanged\": true";
        assert_eq!(
            colorize_diff_line(line, ColorMode::Color),
            line.to_string()
        );
    }

    #[test]
    fn colorize_color_mode_hunk_header_is_cyan() {
        let line = "@@ -1,3 +1,3 @@";
        let out = colorize_diff_line(line, ColorMode::Color);
        assert!(out.contains("\x1b[36m"), "expected cyan in: {out:?}");
    }

    #[test]
    fn colorize_file_headers_are_red_and_green() {
        let minus_hdr = colorize_diff_line("--- local", ColorMode::Color);
        let plus_hdr = colorize_diff_line("+++ remote", ColorMode::Color);
        assert!(minus_hdr.contains("\x1b[91m"), "got: {minus_hdr:?}");
        assert!(plus_hdr.contains("\x1b[92m"), "got: {plus_hdr:?}");
    }

    #[test]
    fn decide_color_mode_no_color_env_returns_plain() {
        assert!(matches!(decide_color_mode(false, true, true), ColorMode::Plain));
        assert!(matches!(decide_color_mode(false, true, false), ColorMode::Plain));
    }

    #[test]
    fn decide_color_mode_no_color_flag_returns_plain() {
        assert!(matches!(decide_color_mode(true, false, true), ColorMode::Plain));
        assert!(matches!(decide_color_mode(true, false, false), ColorMode::Plain));
    }

    #[test]
    fn decide_color_mode_tty_with_no_overrides_returns_color() {
        assert!(matches!(decide_color_mode(false, false, true), ColorMode::Color));
    }

    #[test]
    fn decide_color_mode_no_tty_returns_plain() {
        assert!(matches!(decide_color_mode(false, false, false), ColorMode::Plain));
    }

    #[test]
    fn colorize_prompt_wraps_bracketed_letters() {
        let s = colorize_prompt("[k]eep local  [r]emote", ColorMode::Color);
        // Expect both letters wrapped in bold-cyan SGR.
        assert!(s.matches("\x1b[1;96m").count() == 2, "got: {s:?}");
    }

    #[test]
    fn colorize_prompt_plain_returns_unchanged() {
        let s = colorize_prompt("[k]eep local", ColorMode::Plain);
        assert_eq!(s, "[k]eep local");
    }

    #[test]
    fn prompt_emits_color_codes_when_color_mode() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{\"name\":\"old\"}").unwrap();

        let input = Cursor::new(b"k\n");
        let mut output: Vec<u8> = Vec::new();
        prompt_resolve_with_color(
            input,
            &mut output,
            1,
            1,
            &path,
            b"{\"name\":\"new\"}",
            ColorMode::Color,
        )
        .unwrap();
        let s = String::from_utf8(output).unwrap();
        // Header bold yellow, action letters bold cyan, diff lines red/green.
        assert!(s.contains("\x1b[1;93m"), "no header color: {s:?}");
        assert!(s.contains("\x1b[91m") || s.contains("\x1b[92m"), "no diff color: {s:?}");
        assert!(s.contains("\x1b[1;96m"), "no prompt color: {s:?}");
    }

    #[test]
    fn prompt_plain_mode_emits_no_color_codes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{\"name\":\"old\"}").unwrap();

        let input = Cursor::new(b"k\n");
        let mut output: Vec<u8> = Vec::new();
        prompt_resolve_with_color(
            input,
            &mut output,
            1,
            1,
            &path,
            b"{\"name\":\"new\"}",
            ColorMode::Plain,
        )
        .unwrap();
        let s = String::from_utf8(output).unwrap();
        assert!(!s.contains("\x1b["), "expected no SGR codes: {s:?}");
    }

    #[test]
    fn picker_renders_users_in_priority_order() {
        use crate::model::User;
        let users: Vec<User> = serde_json::from_value(serde_json::json!([
            {"id": 100, "url": "u100", "username": "alice@x", "first_name": "Alice", "last_name": "",
             "is_active": true, "groups": ["https://x/groups/3"]},
            {"id": 938493, "url": "u938493", "username": "system_user__abc", "first_name": "SYS",
             "last_name": "USER", "is_active": true, "groups": ["https://x/groups/3"]},
            {"id": 200, "url": "u200", "username": "bob@x", "first_name": "Bob", "last_name": "",
             "is_active": true, "groups": ["https://x/groups/3"]}
        ])).unwrap();
        let rendered = render_token_owner_picker("master-data-hub", "prod", &users, Some(938493));
        // System user first.
        let sys_pos = rendered.find("u938493").expect("sys user URL in output");
        let alice_pos = rendered.find("u100").expect("alice URL in output");
        assert!(sys_pos < alice_pos, "system_user__ should be ranked first");
        // Active session's own user tagged.
        assert!(rendered.contains("you"), "self user should be tagged");
    }

    #[test]
    fn picker_skips_you_tag_when_self_id_is_none() {
        use crate::model::User;
        let users: Vec<User> = serde_json::from_value(serde_json::json!([
            {"id": 100, "url": "u100", "username": "alice@x", "first_name": "Alice", "last_name": "",
             "is_active": true, "groups": ["https://x/groups/3"]}
        ])).unwrap();
        let rendered = render_token_owner_picker("master-data-hub", "prod", &users, None);
        assert!(!rendered.contains("you"), "no self_id → no 'you' tag");
    }
}
