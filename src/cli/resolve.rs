//! Interactive conflict resolver (spec §8.3).
//!
//! When a three-way pull detects both local and remote diverged from base,
//! the resolver presents an inline prompt:
//!
//! ```text
//! [1/N]  hooks/validator-invoices.json
//!
//! local has changes:
//!   <unified diff snippet>
//!
//! production has changes:
//!   <unified diff snippet>
//!
//! [k] keep local   [r] use production   [e] edit   [s] skip   [a] abort >
//! ```
//!
//! `[k]` keeps local, no write. `[r]` writes remote. `[e]` opens `$EDITOR`
//! on a temp file (with conflict markers) and uses the saved bytes. `[s]`
//! falls through to the original shadow-file behavior (writes
//! `<file>.<env>`, keeps local). `[a]` bubbles a `PullAborted` error so
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
    /// `<file>.<env>`, keep local. Lockfile records local hash.
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
    writeln!(out, "--- {label_a}").expect("writing to String never fails");
    writeln!(out, "+++ {label_b}").expect("writing to String never fails");
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

/// Reshape bytes for diff display. When the bytes parse as JSON, return a
/// stable pretty-printed form (2-space indent, BTreeMap-ordered keys, trailing
/// newline) so per-field changes show on their own diff lines. Non-JSON inputs
/// (`.py` files, raw formula bytes) pass through unchanged.
///
/// On-disk snapshots and the canonical hash projection both store JSON in
/// compact (single-line) form. Diffing that directly produces a one-line
/// "everything changed" diff which is unreadable — this helper is what makes
/// the conflict resolver show the actual field-level change.
pub fn prettify_json_for_diff(bytes: &[u8]) -> Vec<u8> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    let Ok(mut pretty) = serde_json::to_vec_pretty(&value) else {
        return bytes.to_vec();
    };
    if !pretty.ends_with(b"\n") {
        pretty.push(b'\n');
    }
    pretty
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
    env: &str,
) -> Result<Resolution> {
    let mode = detect_color_mode(false);
    prompt_resolve_with_color(input, output, index, total, local_path, remote_bytes, env, mode)
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
    env: &str,
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

    // Pretty-print JSON inputs so each field lands on its own line — without
    // this, the entire compact JSON object renders as a single diff line and
    // the actual change gets buried.
    let local_display = prettify_json_for_diff(&local_canonical);
    let remote_display = prettify_json_for_diff(&remote_canonical);
    let diff = unified_diff("local", &local_display, env, &remote_display);
    if diff.is_empty() {
        return Ok(Resolution::KeepLocal);
    }
    for line in diff.lines() {
        writeln!(output, "{}", colorize_diff_line(line, mode))?;
    }
    writeln!(output)?;

    loop {
        let prompt_text = format!(
            "[k] keep local  [r] use {env}  [e] edit  [s] skip (shadow file)  [a] abort > "
        );
        write!(output, "{}", colorize_prompt(&prompt_text, mode))?;
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
                match run_editor_loop(
                    &mut input,
                    &mut output,
                    &local_bytes,
                    remote_bytes,
                    local_path,
                    mode,
                )? {
                    EditOutcome::Edited(edited) => return Ok(Resolution::Edit(edited)),
                    EditOutcome::Aborted => continue,
                }
            }
            _ => {
                writeln!(output, "  (unrecognized — pick one of k/r/e/s/a)")?;
                continue;
            }
        }
    }
}

/// Result of the editor loop. `Aborted` means the user backed out of the
/// edit without producing usable bytes; the resolver falls back to the
/// main prompt so they can pick keep-local/remote/skip/abort instead.
enum EditOutcome {
    Edited(Vec<u8>),
    Aborted,
}

/// Open `$EDITOR` on a temp file pre-populated with git-style conflict
/// markers (pretty-printed JSON between them, so each field lands on
/// its own line). After every save:
///
/// - If conflict markers are still present, or the file no longer parses
///   as JSON for a `.json` path, show a clear message and offer
///   `[e]dit again / [a]bort` — the user's previous edits are preserved
///   across re-tries.
/// - Otherwise, return the bytes.
fn run_editor_loop<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    local: &[u8],
    remote: &[u8],
    local_path: &Path,
    mode: ColorMode,
) -> Result<EditOutcome> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

    let ext = local_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("tmp");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("rdc-conflict-{stamp}.{ext}"));

    // Pretty-print JSON content so each field is its own editor line.
    let local_view = prettify_json_for_diff(local);
    let remote_view = prettify_json_for_diff(remote);

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"<<<<<<< local\n");
    buf.extend_from_slice(&local_view);
    if !local_view.ends_with(b"\n") {
        buf.push(b'\n');
    }
    buf.extend_from_slice(b"=======\n");
    buf.extend_from_slice(&remote_view);
    if !remote_view.ends_with(b"\n") {
        buf.push(b'\n');
    }
    buf.extend_from_slice(b">>>>>>> remote\n");

    std::fs::write(&path, &buf)
        .with_context(|| format!("writing temp conflict file {}", path.display()))?;

    let result = (|| -> Result<EditOutcome> {
        loop {
            let status = Command::new(&editor)
                .arg(&path)
                .status()
                .with_context(|| format!("spawning editor '{editor}'"))?;
            if !status.success() {
                anyhow::bail!("editor '{editor}' exited with non-zero status");
            }
            let edited = std::fs::read(&path)
                .with_context(|| format!("reading edited conflict file {}", path.display()))?;
            match validate_edited(&edited, local_path) {
                Ok(()) => return Ok(EditOutcome::Edited(edited)),
                Err(reason) => {
                    writeln!(output)?;
                    writeln!(output, "  ✗ {reason}")?;
                    write!(
                        output,
                        "{}",
                        colorize_prompt("  [e]dit again  [a]bort edit > ", mode)
                    )?;
                    output.flush().ok();
                    let mut line = String::new();
                    if input.read_line(&mut line)? == 0 {
                        return Ok(EditOutcome::Aborted);
                    }
                    match line.trim().chars().next() {
                        Some('e') | Some('E') => continue,
                        _ => return Ok(EditOutcome::Aborted),
                    }
                }
            }
        }
    })();
    let _ = std::fs::remove_file(&path);
    result
}

/// Check that an edited conflict file is fit to use: no unresolved
/// markers, valid UTF-8, and valid JSON if the target path is a `.json`
/// file. Returns the failure reason as a user-facing string.
fn validate_edited(bytes: &[u8], local_path: &Path) -> std::result::Result<(), String> {
    let s = std::str::from_utf8(bytes)
        .map_err(|_| "edited file is not valid UTF-8".to_string())?;
    for marker in ["<<<<<<<", "=======", ">>>>>>>"] {
        if s.lines().any(|l| l.starts_with(marker)) {
            return Err(format!(
                "edited file still has the `{marker}` conflict marker — \
                 remove the markers and one of the two sides, then save"
            ));
        }
    }
    if local_path.extension().and_then(|e| e.to_str()) == Some("json")
        && let Err(e) = serde_json::from_str::<serde_json::Value>(s)
    {
        return Err(format!(
            "edited file is not valid JSON ({e}) — fix the syntax and save"
        ));
    }
    Ok(())
}

/// Resolve a single sub-file within a combined-hash entity (hook
/// `.json`/`.py`, schema `schema.json`/formulas/`<id>.py`). Spec §8.3.
///
/// The caller passes the in-memory bytes for both sides. Behavior:
///
/// - `local_bytes == remote_bytes` → no-op (no prompt, no write); returns
///   `local_bytes`.
/// - `interactive == false` → legacy shadow-file: writes
///   `<local_path>.<env>`, keeps local on disk, returns `local_bytes`.
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
    env: &str,
) -> Result<Vec<u8>> {
    use crate::snapshot::writer::write_atomic;

    if local_bytes == remote_bytes {
        return Ok(local_bytes.to_vec());
    }

    if !interactive {
        let conflict_path = shadow_path_for(local_path, env);
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
        env,
    )?;
    match resolution {
        Resolution::KeepLocal => Ok(local_bytes.to_vec()),
        Resolution::KeepRemote => {
            write_atomic(local_path, remote_bytes)?;
            Ok(remote_bytes.to_vec())
        }
        Resolution::Edit(edited) => {
            // Defense in depth — the editor loop already validates, but a
            // second check here means a regression in that path can never
            // turn the local snapshot into unparseable bytes. Local edits
            // are preserved (caller sees the error and the file on disk
            // is whatever was there before).
            if let Err(reason) = validate_edited(&edited, local_path) {
                anyhow::bail!(
                    "refusing to overwrite {} with invalid edit ({}); local file left untouched",
                    local_path.display(),
                    reason
                );
            }
            write_atomic(local_path, &edited)?;
            Ok(edited)
        }
        Resolution::Skip => {
            let conflict_path = shadow_path_for(local_path, env);
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

/// Compute the `<file>.<env>` shadow path for a given local file. The env
/// suffix disambiguates the shadow artifact when a project has multiple envs.
fn shadow_path_for(local_path: &Path, env: &str) -> std::path::PathBuf {
    let mut conflict_path = local_path.to_path_buf();
    let new_name = match conflict_path.file_name().and_then(|s| s.to_str()) {
        Some(name) => format!("{name}.{env}"),
        None => format!("shadow.{env}"),
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
    env: &str,
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
        env,
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

/// Build the per-user labels for the token_owner picker. Users come back
/// in priority order (system → admin → other) so the recommended pick is
/// at the top. Each label is a single-line summary that fits in an
/// inquire `Select`.
pub fn format_user_choices(
    users: &[crate::model::User],
    self_user_id: Option<u64>,
) -> Vec<String> {
    let sorted = sort_users_for_picker(users);
    sorted
        .iter()
        .map(|u| {
            let mut tags = Vec::new();
            if u.is_admin() {
                tags.push("admin");
            }
            if u.is_active {
                tags.push("active");
            }
            if Some(u.id) == self_user_id {
                tags.push("you");
            }
            let tags = tags.join(", ");
            let display = if u.first_name.is_empty() && u.last_name.is_empty() {
                u.username.clone()
            } else {
                format!("{} {}", u.first_name, u.last_name).trim().to_string()
            };
            format!("{display}   [{tags}]   {}", u.url)
        })
        .collect()
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
    use inquire::error::InquireError;
    use inquire::{Confirm, Select};

    let sorted = sort_users_for_picker(users);
    let mut options = format_user_choices(users, self_user_id);
    let abort_label = "abort the deploy".to_string();
    options.push(abort_label.clone());

    let prompt = format!("Pick the token_owner for store extension '{slug}' on {tgt_env}");
    let help =
        "used as the API service account for the extension's calls (usually a system user)";

    let answer = match Select::new(&prompt, options.clone())
        .with_help_message(help)
        .raw_prompt()
    {
        Ok(opt) => opt,
        Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => {
            return Ok(None);
        }
        Err(e) => return Err(anyhow::anyhow!("prompt failed: {e}")),
    };

    if answer.value == abort_label {
        return Ok(None);
    }
    let chosen = sorted
        .get(answer.index)
        .ok_or_else(|| anyhow::anyhow!("internal: picker index {} out of range", answer.index))?;

    let apply_all = Confirm::new("Apply this choice to all remaining store extensions in this deploy?")
        .with_default(false)
        .prompt()
        .unwrap_or(false);
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

// Truecolor (24-bit) SGR escapes shared across resolver/diff output.
// The palette matches `cli::mod`'s clap styling: warm amber accent for
// emphasis (`@@` hunk headers, conflict headers, action-letter brackets),
// soft red for removed lines, sage green for added — chosen for
// contrast on both light and dark terminal themes.
const SGR_RESET: &str = "\x1b[0m";
const SGR_AMBER: &str = "\x1b[38;2;237;142;71m";
const SGR_AMBER_BOLD: &str = "\x1b[1;38;2;237;142;71m";
const SGR_REMOVE: &str = "\x1b[38;2;220;80;80m";
const SGR_REMOVE_BOLD: &str = "\x1b[1;38;2;220;80;80m";
const SGR_ADD: &str = "\x1b[38;2;120;180;90m";
const SGR_ADD_BOLD: &str = "\x1b[1;38;2;120;180;90m";

/// Apply color to a single line of unified-diff output. Returns `line`
/// unchanged in [`ColorMode::Plain`].
pub fn colorize_diff_line(line: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return line.to_string();
    }
    let prefix = if line.starts_with("--- ") {
        SGR_REMOVE_BOLD
    } else if line.starts_with("+++ ") {
        SGR_ADD_BOLD
    } else if line.starts_with("@@") {
        SGR_AMBER
    } else if line.starts_with('-') {
        SGR_REMOVE
    } else if line.starts_with('+') {
        SGR_ADD
    } else {
        return line.to_string();
    };
    format!("{prefix}{line}{SGR_RESET}")
}

/// Colorize the conflict header line in bold amber — matches the
/// primary accent used by clap headers and prompt brackets.
pub fn colorize_header(text: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return text.to_string();
    }
    format!("{SGR_AMBER_BOLD}{text}{SGR_RESET}")
}

/// Colorize the action-letter prompt line. Bracketed single-letter tokens
/// like `[k]` are wrapped in bold amber; the rest of the prompt is unchanged.
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
                        out.push('[');
                        out.push_str(SGR_AMBER_BOLD);
                        out.push(letter);
                        out.push_str(SGR_RESET);
                        out.push(']');
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
    fn prettify_splits_compact_json_into_per_field_lines() {
        let pretty = prettify_json_for_diff(br#"{"name":"x","status":"ready"}"#);
        let s = String::from_utf8(pretty).unwrap();
        assert!(s.contains("\"name\""));
        assert!(s.contains("\"status\""));
        // Two top-level fields => at least two lines plus braces.
        assert!(s.lines().count() >= 4, "got: {s:?}");
    }

    #[test]
    fn prettify_passes_through_non_json() {
        let py = b"def main():\n    pass\n";
        assert_eq!(prettify_json_for_diff(py), py.to_vec());
    }

    #[test]
    fn diff_of_two_compact_json_objects_shows_per_field_lines() {
        let local = br#"{"name":"x","status":"ready"}"#;
        let remote = br#"{"name":"x","status":"pending"}"#;
        let l = prettify_json_for_diff(local);
        let r = prettify_json_for_diff(remote);
        let d = unified_diff("local", &l, "remote", &r);
        // The actual change (`status` field) must appear, and the
        // unchanged `name` line must not be a `-`/`+` line.
        assert!(d.contains("-  \"status\": \"ready\""), "got: {d}");
        assert!(d.contains("+  \"status\": \"pending\""), "got: {d}");
        assert!(!d.contains("-  \"name\""), "name should be in context only, got: {d}");
    }

    #[test]
    fn validate_edited_rejects_unresolved_markers() {
        let body = b"{\n<<<<<<< local\n  \"a\": 1\n=======\n  \"a\": 2\n>>>>>>> remote\n}\n";
        let err = validate_edited(body, std::path::Path::new("x.json")).unwrap_err();
        assert!(err.contains("conflict marker"), "got: {err}");
    }

    #[test]
    fn validate_edited_rejects_invalid_json_for_json_path() {
        let body = b"{not json}\n";
        let err = validate_edited(body, std::path::Path::new("x.json")).unwrap_err();
        assert!(err.contains("not valid JSON"), "got: {err}");
    }

    #[test]
    fn validate_edited_accepts_valid_json() {
        let body = b"{\n  \"a\": 1\n}\n";
        validate_edited(body, std::path::Path::new("x.json")).unwrap();
    }

    #[test]
    fn validate_edited_skips_json_check_for_py_path() {
        let body = b"def main():\n    pass\n";
        validate_edited(body, std::path::Path::new("x.py")).unwrap();
    }

    #[test]
    fn prompt_keep_local_returns_keep_local() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();

        let input = Cursor::new(b"k\n");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"remote\n", "test").unwrap();
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
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"remote\n", "test").unwrap();
        assert!(matches!(r, Resolution::KeepRemote));
    }

    #[test]
    fn prompt_skip_returns_skip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();

        let input = Cursor::new(b"s\n");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 2, 5, &path, b"remote\n", "test").unwrap();
        assert!(matches!(r, Resolution::Skip));
    }

    #[test]
    fn prompt_abort_returns_abort() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();

        let input = Cursor::new(b"a\n");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"remote\n", "test").unwrap();
        assert!(matches!(r, Resolution::Abort));
    }

    #[test]
    fn prompt_unrecognized_re_prompts_then_accepts() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();

        let input = Cursor::new(b"q\nx\n\nk\n");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"remote\n", "test").unwrap();
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
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"remote\n", "test").unwrap();
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
        let r = prompt_resolve(input, &mut output, 1, 1, &path, b"same\n", "test").unwrap();
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
        let out = resolve_combined_file(1, 2, &path, b"same\n", b"same\n", true, "test").unwrap();
        assert_eq!(out, b"same\n");
        // No shadow file written.
        assert!(!dir.path().join("a.py.test").exists());
    }

    #[test]
    fn resolve_combined_file_writes_shadow_when_non_interactive() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.py");
        std::fs::write(&path, b"local\n").unwrap();
        let out = resolve_combined_file(1, 1, &path, b"local\n", b"remote\n", false, "test").unwrap();
        assert_eq!(out, b"local\n");
        assert_eq!(std::fs::read(dir.path().join("a.py.test")).unwrap(), b"remote\n");
        // Local file untouched.
        assert_eq!(std::fs::read(&path).unwrap(), b"local\n");
    }

    #[test]
    fn shadow_path_inserts_env_suffix() {
        let p = std::path::PathBuf::from("/tmp/x/y.json");
        assert_eq!(shadow_path_for(&p, "dev"), std::path::PathBuf::from("/tmp/x/y.json.dev"));
    }

    #[test]
    fn shadow_path_for_py_extension() {
        let p = std::path::PathBuf::from("/tmp/formulas/123.py");
        assert_eq!(shadow_path_for(&p, "production"), std::path::PathBuf::from("/tmp/formulas/123.py.production"));
    }

    #[test]
    fn resolve_push_drift_non_interactive_returns_skip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local\n").unwrap();
        let r = resolve_push_drift(false, &path, b"remote\n", "test").unwrap();
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
            "test",
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
    fn colorize_color_mode_renders_minus_in_remove_hue() {
        let line = "-  \"name\": \"old\"";
        let out = colorize_diff_line(line, ColorMode::Color);
        // Truecolor SGR for the "remove" hue used by `-` lines.
        assert!(out.contains(SGR_REMOVE), "expected remove hue in: {out:?}");
        assert!(out.ends_with(SGR_RESET), "expected reset suffix in: {out:?}");
    }

    #[test]
    fn colorize_color_mode_renders_plus_in_add_hue() {
        let line = "+  \"name\": \"new\"";
        let out = colorize_diff_line(line, ColorMode::Color);
        assert!(out.contains(SGR_ADD), "expected add hue in: {out:?}");
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
    fn colorize_color_mode_hunk_header_is_amber() {
        let line = "@@ -1,3 +1,3 @@";
        let out = colorize_diff_line(line, ColorMode::Color);
        assert!(out.contains(SGR_AMBER), "expected amber accent in: {out:?}");
    }

    #[test]
    fn colorize_file_headers_use_bold_remove_and_add_hues() {
        let minus_hdr = colorize_diff_line("--- local", ColorMode::Color);
        let plus_hdr = colorize_diff_line("+++ remote", ColorMode::Color);
        assert!(minus_hdr.contains(SGR_REMOVE_BOLD), "got: {minus_hdr:?}");
        assert!(plus_hdr.contains(SGR_ADD_BOLD), "got: {plus_hdr:?}");
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
        // Both letters get wrapped in the bold-amber accent.
        assert!(s.matches(SGR_AMBER_BOLD).count() == 2, "got: {s:?}");
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
            "test",
            ColorMode::Color,
        )
        .unwrap();
        let s = String::from_utf8(output).unwrap();
        // Conflict header in bold amber, action letters in bold amber,
        // diff lines in remove/add hues.
        assert!(s.contains(SGR_AMBER_BOLD), "no amber accent: {s:?}");
        assert!(s.contains(SGR_REMOVE) || s.contains(SGR_ADD), "no diff hue: {s:?}");
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
            "test",
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
        let choices = format_user_choices(&users, Some(938493));
        // System user first.
        assert!(choices[0].contains("u938493"), "system_user should be ranked first, got {:?}", choices);
        assert!(choices.iter().any(|c| c.contains("u100")), "alice should be present");
        // Active session's own user tagged.
        assert!(choices[0].contains("you"), "self user should be tagged, got {:?}", choices[0]);
    }

    #[test]
    fn picker_skips_you_tag_when_self_id_is_none() {
        use crate::model::User;
        let users: Vec<User> = serde_json::from_value(serde_json::json!([
            {"id": 100, "url": "u100", "username": "alice@x", "first_name": "Alice", "last_name": "",
             "is_active": true, "groups": ["https://x/groups/3"]}
        ])).unwrap();
        let choices = format_user_choices(&users, None);
        assert!(!choices[0].contains("you"), "no self_id → no 'you' tag, got {:?}", choices[0]);
    }

    #[test]
    fn prompt_resolve_uses_env_name_in_labels() {
        use std::io::Cursor;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("x.json");
        std::fs::write(&local, b"{\"a\":1}").unwrap();
        let remote = b"{\"a\":2}";
        let mut out: Vec<u8> = Vec::new();
        let input = Cursor::new(b"s\n");

        let _ = prompt_resolve_with_color(
            input, &mut out, 1, 1, &local, remote, "production", ColorMode::Plain,
        ).unwrap();

        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("[r] use production"), "prompt missing env-named [r] label: {s}");
        assert!(s.contains("+++ production"), "diff header should name the env: {s}");
        assert!(!s.contains("[r]emote"), "old literal label leaked: {s}");
    }
}
