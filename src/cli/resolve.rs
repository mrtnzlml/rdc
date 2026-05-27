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
use similar::{Algorithm, TextDiff};
use std::fmt::Write as FmtWrite;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

/// Build a line-level `TextDiff` using the Histogram algorithm.
///
/// `similar`'s default Myers (and Patience / Lcs) can emit non-contiguous,
/// inconsistent `DiffOp` cursors after `Compact` post-processing on pathological
/// inputs (e.g. long runs of identical blank lines paired with short remotes),
/// which makes `ops()` unwalkable — the hunk walker drops lines, the unified-diff
/// renderer can mis-attribute regions, and `iter_all_changes()` fails to
/// round-trip. Histogram is robust on these cases and is what Git uses by default.
///
/// All `TextDiff::from_lines` call sites in this crate should go through this
/// helper so the resolver UI, hunk walker, and conflict-buffer builder all agree
/// on the same op sequence.
pub fn line_diff<'old, 'new>(old: &'old str, new: &'new str) -> TextDiff<'old, 'new, str> {
    TextDiff::configure().algorithm(Algorithm::Histogram).diff_lines(old, new)
}

/// Outcome of presenting a single conflict to the user.
#[derive(Debug)]
pub enum Resolution {
    /// Keep the local file as-is. No write. Lockfile records local hash.
    KeepLocal,
    /// Overwrite local with the remote bytes. Lockfile records remote hash.
    KeepRemote,
    /// Use these (user-edited) bytes. Lockfile records hash of these bytes.
    Edit(Vec<u8>),
    /// Like `Edit`, but the user explicitly opted into a partial resolution
    /// via `[h]unk-by-hunk → [s]kip` on one or more hunks. The bytes will
    /// contain unresolved `<<<<<<<` / `=======` / `>>>>>>>` markers and the
    /// caller MUST skip marker-leakage validation when writing them. The
    /// committed bytes are still recorded in the lockfile by hash, so a
    /// follow-up pull sees the partial resolution as the new base.
    EditWithMarkers(Vec<u8>),
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
    let diff = line_diff(a_str.as_ref(), b_str.as_ref());
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
    input: R,
    output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    remote_bytes: &[u8],
    env: &str,
    mode: ColorMode,
) -> Result<Resolution> {
    let local_bytes = read_local(local_path)?;
    prompt_resolve_with_bytes_and_color(
        input, output, index, total, local_path, &local_bytes, remote_bytes, env, mode,
    )
}

/// Bytes-driven variant of [`prompt_resolve_with_color`]. Used by the
/// sync executor's conflict resolver when the divergence lives in a
/// sidecar (`.py`, `formulas/<id>.py`) that may not exist on disk
/// (asymmetric case where one side has the sidecar and the other
/// doesn't). Reading from disk would fail; callers compose the
/// `local_bytes` directly and pass them here.
///
/// `local_path` is used only for the prompt header (it appears as
/// "local") and for the `[e]dit` editor's tempfile extension; it does
/// NOT need to exist on disk.
pub fn prompt_resolve_with_bytes<R: BufRead, W: Write>(
    input: R,
    output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    local_bytes: &[u8],
    remote_bytes: &[u8],
    env: &str,
) -> Result<Resolution> {
    let mode = detect_color_mode(false);
    prompt_resolve_with_bytes_and_color(
        input, output, index, total, local_path, local_bytes, remote_bytes, env, mode,
    )
}

/// Color-aware bytes-driven core. Shares the prompt + diff +
/// `[e]dit`/`[h]` plumbing with [`prompt_resolve_with_color`]; the
/// only difference is that local bytes come from the caller, not
/// from `local_path`.
#[allow(clippy::too_many_arguments)]
pub fn prompt_resolve_with_bytes_and_color<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    local_bytes: &[u8],
    remote_bytes: &[u8],
    env: &str,
    mode: ColorMode,
) -> Result<Resolution> {
    // Strip noise fields before diff display so the user only sees real
    // changes. modified_at server-churn must not appear in the resolver.
    let local_canonical = crate::snapshot::noise::canonicalize_for_hash(local_bytes);
    let remote_canonical = crate::snapshot::noise::canonicalize_for_hash(remote_bytes);

    if local_canonical == remote_canonical {
        return Ok(Resolution::KeepLocal);
    }

    // Pretty-print JSON inputs so each field lands on its own line — without
    // this, the entire compact JSON object renders as a single diff line and
    // the actual change gets buried.
    let local_display = prettify_json_for_diff(&local_canonical);
    let remote_display = prettify_json_for_diff(&remote_canonical);

    // Count conflict hunks up-front so the header can advertise them and
    // the action list can offer `[h]` only when multi-hunk.
    let hunk_count = count_conflict_hunks(&local_display, &remote_display);

    writeln!(output)?;
    let header = if hunk_count >= 2 {
        format!(
            "[{index}/{total}]  {} -- conflict ({hunk_count} hunks)",
            local_path.display()
        )
    } else {
        format!("[{index}/{total}]  {} -- conflict", local_path.display())
    };
    writeln!(output, "{}", colorize_header(&header, mode))?;
    writeln!(output)?;

    // Same styled renderer every other diff surface uses, so the conflict
    // preview shares the look. The `env` side is `+` (the prompt already
    // names it: "[r] use {env}"); local is `-`.
    let left = String::from_utf8_lossy(&local_display);
    let right = String::from_utf8_lossy(&remote_display);
    let p = local_path.display();
    let diff = render_styled_diff(
        &format!("{p} (local)"),
        &format!("{p} ({env})"),
        &left,
        &right,
        mode,
    );
    if diff.is_empty() {
        return Ok(Resolution::KeepLocal);
    }
    write!(output, "{diff}")?;
    writeln!(output)?;

    loop {
        let prompt_text = if hunk_count >= 2 {
            format!(
                "[k] keep local  [r] use {env}  [e] edit  [h] hunk-by-hunk  [s] skip (shadow file)  [a] abort > "
            )
        } else {
            format!(
                "[k] keep local  [r] use {env}  [e] edit  [s] skip (shadow file)  [a] abort > "
            )
        };
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
                    local_bytes,
                    remote_bytes,
                    local_path,
                    env,
                    mode,
                )? {
                    EditOutcome::Edited(edited) => return Ok(Resolution::Edit(edited)),
                    EditOutcome::EditedWithMarkers(edited) => {
                        return Ok(Resolution::EditWithMarkers(edited));
                    }
                    EditOutcome::Aborted => continue,
                }
            }
            Some('h') | Some('H') if hunk_count >= 2 => {
                match prompt_hunk_by_hunk(
                    &mut input,
                    &mut output,
                    &local_display,
                    &remote_display,
                    local_path,
                    env,
                    mode,
                )? {
                    EditOutcome::Edited(edited) => return Ok(Resolution::Edit(edited)),
                    EditOutcome::EditedWithMarkers(edited) => {
                        return Ok(Resolution::EditWithMarkers(edited));
                    }
                    EditOutcome::Aborted => continue,
                }
            }
            _ => {
                let hint = if hunk_count >= 2 {
                    "  (unrecognized; pick one of k/r/e/h/s/a)"
                } else {
                    "  (unrecognized; pick one of k/r/e/s/a)"
                };
                writeln!(output, "{hint}")?;
                continue;
            }
        }
    }
}

/// Count the number of conflict hunks (contiguous non-Equal regions) in
/// the line-level diff between `local` and `remote`. Equal regions don't
/// count; a `Replace` (delete + insert pair) is one hunk; isolated
/// `Delete`s or `Insert`s are each one hunk.
fn count_conflict_hunks(local: &[u8], remote: &[u8]) -> usize {
    use similar::DiffTag;
    let local_str = String::from_utf8_lossy(local);
    let remote_str = String::from_utf8_lossy(remote);
    let diff = line_diff(local_str.as_ref(), remote_str.as_ref());
    diff.ops()
        .iter()
        .filter(|op| op.tag() != DiffTag::Equal)
        .count()
}

/// Top-level entry point for the remote-deleted prompt. Auto-detects color
/// mode. The local file is shown as a preview; the env's "deleted" status
/// is asserted in the header. Returns one of:
/// - `Resolution::KeepLocal` — user wants to restore on env (POST it back).
/// - `Resolution::KeepRemote` — user accepts deletion; local file should be removed.
/// - `Resolution::Skip` — write `<file>.<env>-deleted` marker; defer decision.
/// - `Resolution::Abort` — caller bails (e.g. via `PullAborted`).
///
/// `Resolution::Edit` is unreachable from this prompt (no `[e]` option offered).
pub fn prompt_remote_delete<R: BufRead, W: Write>(
    input: R,
    output: W,
    local_path: &Path,
    env: &str,
) -> Result<Resolution> {
    let mode = detect_color_mode(false);
    prompt_remote_delete_with_color(input, output, local_path, env, mode)
}

/// Color-aware variant. Tests pin the mode; production goes through
/// `prompt_remote_delete`.
pub fn prompt_remote_delete_with_color<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    local_path: &Path,
    env: &str,
    mode: ColorMode,
) -> Result<Resolution> {
    let local_bytes = read_local(local_path)?;
    let preview = prettify_json_for_diff(&local_bytes);

    writeln!(output)?;
    let header = format!("{} -- deleted on {env}", local_path.display());
    writeln!(output, "{}", colorize_header(&header, mode))?;
    writeln!(output)?;
    writeln!(output, "local has the file:")?;

    // Elide the preview to ~40 lines for unwieldy bodies. The spec's
    // open question allows this; revisit if user feedback says otherwise.
    let s = String::from_utf8_lossy(&preview);
    let lines: Vec<&str> = s.lines().collect();
    let limit = 40;
    if lines.len() <= limit {
        for ln in &lines {
            writeln!(output, "  {ln}")?;
        }
    } else {
        for ln in &lines[..limit] {
            writeln!(output, "  {ln}")?;
        }
        writeln!(output, "  ... ({} more lines)", lines.len() - limit)?;
    }
    writeln!(output)?;
    writeln!(output, "{env} has it deleted.")?;
    writeln!(output)?;

    loop {
        let prompt_text = format!(
            "[k] keep local (restore on {env})  \
             [r] use {env} (delete local)  \
             [s] skip  \
             [a] abort > "
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
            _ => {
                writeln!(output, "  (unrecognized; pick one of k/r/s/a)")?;
                continue;
            }
        }
    }
}

/// Result of the editor loop. `Aborted` means the user backed out of the
/// edit without producing usable bytes; the resolver falls back to the
/// main prompt so they can pick keep-local/remote/skip/abort instead.
///
/// `EditedWithMarkers` is only produced by the hunk-by-hunk walker when
/// at least one hunk was resolved via `[s]kip`; it tells the caller to
/// bypass marker-leakage validation when writing the bytes.
#[derive(Debug)]
enum EditOutcome {
    Edited(Vec<u8>),
    EditedWithMarkers(Vec<u8>),
    Aborted,
}

/// Build a git-style merge-conflict buffer for `$EDITOR`. Only differing
/// hunks are wrapped in `<<<<<<< local / ======= / >>>>>>> {env}` markers;
/// identical lines pass through unchanged.
///
/// Operates on already-prettified bytes (pretty-printed JSON or raw `.py`).
fn build_conflict_buffer(local: &[u8], remote: &[u8], env: &str) -> Vec<u8> {
    use similar::ChangeTag;

    let local_str = String::from_utf8_lossy(local);
    let remote_str = String::from_utf8_lossy(remote);
    let diff = line_diff(local_str.as_ref(), remote_str.as_ref());

    let mut out = String::new();
    let mut local_chunk: Vec<&str> = Vec::new();
    let mut remote_chunk: Vec<&str> = Vec::new();

    fn flush<'a>(
        out: &mut String,
        local: &mut Vec<&'a str>,
        remote: &mut Vec<&'a str>,
        env: &str,
    ) {
        if local.is_empty() && remote.is_empty() {
            return;
        }
        out.push_str("<<<<<<< local\n");
        for l in local.drain(..) {
            out.push_str(l);
            if !l.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push_str("=======\n");
        for r in remote.drain(..) {
            out.push_str(r);
            if !r.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push_str(">>>>>>> ");
        out.push_str(env);
        out.push('\n');
    }

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                flush(&mut out, &mut local_chunk, &mut remote_chunk, env);
                out.push_str(change.value());
            }
            ChangeTag::Delete => {
                local_chunk.push(change.value());
            }
            ChangeTag::Insert => {
                remote_chunk.push(change.value());
            }
        }
    }
    flush(&mut out, &mut local_chunk, &mut remote_chunk, env);

    // Defensive: ensure trailing newline (matches what the editor expects).
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }

    out.into_bytes()
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
    env: &str,
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

    let buf = build_conflict_buffer(&local_view, &remote_view, env);

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
                    writeln!(output, "  [fail] {reason}")?;
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
    // Markers are caught even when indented — leading whitespace on a
    // marker line is almost never legitimate content in `.py` or `.json`
    // files, and a sneakily-indented `    <<<<<<<` would otherwise slip
    // through into the lockfile/snapshot.
    for marker in ["<<<<<<<", "=======", ">>>>>>>"] {
        if s.lines().any(|l| l.trim_start().starts_with(marker)) {
            return Err(format!(
                "edited file still has the `{marker}` conflict marker; \
                 remove the markers and one of the two sides, then save"
            ));
        }
    }
    if local_path.extension().and_then(|e| e.to_str()) == Some("json")
        && let Err(e) = serde_json::from_str::<serde_json::Value>(s)
    {
        return Err(format!(
            "edited file is not valid JSON ({e}); fix the syntax and save"
        ));
    }
    Ok(())
}

/// Walk each conflict hunk in order, asking the user for a per-hunk
/// decision. Equal regions pass through unchanged; non-Equal regions are
/// resolved per the user's choice (`[k]`/`[r]`/`[e]`/`[b]`/`[s]`/`[a]`).
///
/// `local` and `remote` are the already-prettified byte slices (same form
/// used by the main resolver's diff display) so per-field JSON lands on
/// its own line.
///
/// Returns:
/// - `EditOutcome::Edited(bytes)` — all hunks resolved cleanly; bytes
///   contain no marker leakage.
/// - `EditOutcome::EditedWithMarkers(bytes)` — at least one hunk was
///   resolved via `[s]kip`, so the output retains `<<<<<<< local /
///   ======= / >>>>>>> {env}` markers around that hunk. Caller MUST
///   skip the marker-leakage check when writing.
/// - `EditOutcome::Aborted` — user picked `[a]` at some hunk; caller
///   falls back to the main prompt.
fn prompt_hunk_by_hunk<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    local: &[u8],
    remote: &[u8],
    local_path: &Path,
    env: &str,
    mode: ColorMode,
) -> Result<EditOutcome> {
    use similar::DiffTag;

    let local_str = String::from_utf8_lossy(local);
    let remote_str = String::from_utf8_lossy(remote);
    let diff = line_diff(local_str.as_ref(), remote_str.as_ref());

    // Index lines by their position in each side so we can slice them
    // out by `old_range()` / `new_range()` from each DiffOp.
    let local_lines: Vec<&str> = diff.iter_old_slices().collect();
    let remote_lines: Vec<&str> = diff.iter_new_slices().collect();

    let ops: Vec<_> = diff.ops().to_vec();
    let conflict_total = ops.iter().filter(|op| op.tag() != DiffTag::Equal).count();

    // Defensive: caller already short-circuits when there are no conflict
    // hunks (e.g. local == remote). If somehow reached, return local bytes.
    if conflict_total == 0 {
        return Ok(EditOutcome::Edited(local.to_vec()));
    }

    let mut merged = String::new();
    let mut any_skipped = false;
    let mut hunk_idx = 0usize; // 1-based index reported to the user, incremented when entering a hunk

    for op in &ops {
        match op.tag() {
            DiffTag::Equal => {
                for line in &local_lines[op.old_range()] {
                    merged.push_str(line);
                    if !line.ends_with('\n') {
                        merged.push('\n');
                    }
                }
            }
            _ => {
                hunk_idx += 1;
                let local_slice = &local_lines[op.old_range()];
                let remote_slice = &remote_lines[op.new_range()];

                let outcome = prompt_single_hunk(
                    input,
                    output,
                    hunk_idx,
                    conflict_total,
                    &local_lines,
                    op.old_range(),
                    &remote_lines,
                    op.new_range(),
                    local_path,
                    env,
                    mode,
                )?;

                match outcome {
                    HunkOutcome::Keep => append_lines(&mut merged, local_slice),
                    HunkOutcome::Remote => append_lines(&mut merged, remote_slice),
                    HunkOutcome::Both => {
                        append_lines(&mut merged, local_slice);
                        append_lines(&mut merged, remote_slice);
                    }
                    HunkOutcome::Edit(bytes) => {
                        let s = String::from_utf8_lossy(&bytes);
                        merged.push_str(&s);
                        if !s.ends_with('\n') && !s.is_empty() {
                            merged.push('\n');
                        }
                    }
                    HunkOutcome::Skip => {
                        any_skipped = true;
                        merged.push_str("<<<<<<< local\n");
                        append_lines(&mut merged, local_slice);
                        merged.push_str("=======\n");
                        append_lines(&mut merged, remote_slice);
                        merged.push_str(">>>>>>> ");
                        merged.push_str(env);
                        merged.push('\n');
                    }
                    HunkOutcome::Abort => return Ok(EditOutcome::Aborted),
                }
            }
        }
    }

    let bytes = merged.into_bytes();
    if any_skipped {
        Ok(EditOutcome::EditedWithMarkers(bytes))
    } else {
        Ok(EditOutcome::Edited(bytes))
    }
}

/// Per-hunk decision returned by [`prompt_single_hunk`].
enum HunkOutcome {
    /// Emit the local lines as-is.
    Keep,
    /// Emit the remote lines as-is.
    Remote,
    /// Emit local lines followed by remote lines (no markers).
    Both,
    /// Emit these user-edited bytes in place of the hunk.
    Edit(Vec<u8>),
    /// Wrap the hunk in conflict markers so the user can resolve later.
    Skip,
    /// Bubble abort to the walker.
    Abort,
}

/// Append a list of line slices to `out`, ensuring each ends in `\n`.
fn append_lines(out: &mut String, lines: &[&str]) {
    for line in lines {
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push('\n');
        }
    }
}

/// Render the per-hunk prompt and read the user's decision for a single
/// hunk. The user can pick keep / remote / both / edit / skip / abort.
///
/// The display shows up to `CONTEXT` lines of equal context before and
/// after the differing hunk so the user can orient themselves in the
/// file. Equal lines are taken from `local_lines` and follow the
/// unified-diff convention (leading space); the hunk's removed / added
/// lines use `-` / `+`.
#[allow(clippy::too_many_arguments)]
fn prompt_single_hunk<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    hunk_idx: usize,
    hunk_total: usize,
    local_lines: &[&str],
    local_range: std::ops::Range<usize>,
    remote_lines: &[&str],
    remote_range: std::ops::Range<usize>,
    local_path: &Path,
    env: &str,
    mode: ColorMode,
) -> Result<HunkOutcome> {
    /// Lines of equal context to render before and after the hunk.
    const CONTEXT: usize = 3;

    let local_slice = &local_lines[local_range.clone()];
    let remote_slice = &remote_lines[remote_range.clone()];

    // Equal context comes from `local_lines` — by definition those
    // lines are identical on the remote side (anything different would
    // be inside a non-Equal hunk).
    let prefix_start = local_range.start.saturating_sub(CONTEXT);
    let prefix = &local_lines[prefix_start..local_range.start];

    let suffix_end = local_range.end.saturating_add(CONTEXT).min(local_lines.len());
    let suffix = &local_lines[local_range.end..suffix_end];

    writeln!(output)?;
    // Line numbers are 1-based and inclusive. An empty range (pure Insert
    // on the local side) still has start == end; show "after line N" instead.
    let line_range = if local_range.is_empty() {
        format!("after line {}", local_range.start)
    } else {
        let start = local_range.start + 1;
        let end = local_range.end;
        if start == end {
            format!("line {start}")
        } else {
            format!("lines {start}-{end}")
        }
    };
    let header = format!(
        "[hunk {hunk_idx}/{hunk_total}]  {}  ({line_range})",
        local_path.display()
    );
    writeln!(output, "{}", colorize_header(&header, mode))?;

    // Context before the hunk: unified-diff convention prefixes equal
    // lines with a single space (no color).
    for line in prefix {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        writeln!(output, " {stripped}")?;
    }
    for line in local_slice {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let formatted = format!("-{stripped}");
        writeln!(output, "{}", colorize_diff_line(&formatted, mode))?;
    }
    for line in remote_slice {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let formatted = format!("+{stripped}");
        writeln!(output, "{}", colorize_diff_line(&formatted, mode))?;
    }
    // Context after the hunk.
    for line in suffix {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        writeln!(output, " {stripped}")?;
    }
    writeln!(output)?;

    loop {
        let prompt_text = format!(
            "[k] keep local  [r] use {env}  [e] edit  [b] both  [s] skip  [a] abort > "
        );
        write!(output, "{}", colorize_prompt(&prompt_text, mode))?;
        output.flush().ok();
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            // EOF mid-walk → treat like skip so the partial result is
            // still preserved with markers; safer than silently keeping
            // local on a half-typed answer.
            return Ok(HunkOutcome::Skip);
        }
        match line.trim().chars().next() {
            Some('k') | Some('K') => return Ok(HunkOutcome::Keep),
            Some('r') | Some('R') => return Ok(HunkOutcome::Remote),
            Some('b') | Some('B') => return Ok(HunkOutcome::Both),
            Some('s') | Some('S') => return Ok(HunkOutcome::Skip),
            Some('a') | Some('A') => return Ok(HunkOutcome::Abort),
            Some('e') | Some('E') => {
                match run_single_hunk_editor(
                    input,
                    output,
                    local_slice,
                    remote_slice,
                    local_path,
                    env,
                    mode,
                )? {
                    Some(bytes) => return Ok(HunkOutcome::Edit(bytes)),
                    None => continue,
                }
            }
            _ => {
                writeln!(output, "  (unrecognized; pick one of k/r/e/b/s/a)")?;
                continue;
            }
        }
    }
}

/// Open `$EDITOR` on a temp file containing just this hunk's local
/// section, `=======`, and remote section (with `<<<<<<< local` /
/// `>>>>>>> {env}` markers). Validates that the saved bytes don't
/// reintroduce conflict markers. JSON validation is *not* applied here
/// because a single hunk usually isn't a full JSON document.
///
/// Returns `Some(bytes)` on a successful edit, or `None` if the user
/// aborted the editor sub-loop (in which case the walker re-prompts for
/// this hunk).
fn run_single_hunk_editor<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    local_slice: &[&str],
    remote_slice: &[&str],
    local_path: &Path,
    env: &str,
    mode: ColorMode,
) -> Result<Option<Vec<u8>>> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

    let ext = local_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("tmp");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("rdc-hunk-{stamp}.{ext}"));

    let mut buf = String::new();
    buf.push_str("<<<<<<< local\n");
    append_lines(&mut buf, local_slice);
    buf.push_str("=======\n");
    append_lines(&mut buf, remote_slice);
    buf.push_str(">>>>>>> ");
    buf.push_str(env);
    buf.push('\n');

    std::fs::write(&path, buf.as_bytes())
        .with_context(|| format!("writing temp hunk file {}", path.display()))?;

    let result = (|| -> Result<Option<Vec<u8>>> {
        loop {
            let status = Command::new(&editor)
                .arg(&path)
                .status()
                .with_context(|| format!("spawning editor '{editor}'"))?;
            if !status.success() {
                anyhow::bail!("editor '{editor}' exited with non-zero status");
            }
            let edited = std::fs::read(&path)
                .with_context(|| format!("reading edited hunk file {}", path.display()))?;
            match validate_edited_markers_only(&edited) {
                Ok(()) => return Ok(Some(edited)),
                Err(reason) => {
                    writeln!(output)?;
                    writeln!(output, "  [fail] {reason}")?;
                    write!(
                        output,
                        "{}",
                        colorize_prompt("  [e]dit again  [a]bort edit > ", mode)
                    )?;
                    output.flush().ok();
                    let mut line = String::new();
                    if input.read_line(&mut line)? == 0 {
                        return Ok(None);
                    }
                    match line.trim().chars().next() {
                        Some('e') | Some('E') => continue,
                        _ => return Ok(None),
                    }
                }
            }
        }
    })();
    let _ = std::fs::remove_file(&path);
    result
}

/// Subset of [`validate_edited`] that only checks for marker leakage; the
/// JSON-syntax check is intentionally skipped because per-hunk edits
/// don't carry a full JSON document context.
fn validate_edited_markers_only(bytes: &[u8]) -> std::result::Result<(), String> {
    let s = std::str::from_utf8(bytes)
        .map_err(|_| "edited hunk is not valid UTF-8".to_string())?;
    // Match `validate_edited`: catch indented markers too. A user could
    // otherwise leave `    <<<<<<<` inside a per-hunk edit and quietly
    // commit it.
    for marker in ["<<<<<<<", "=======", ">>>>>>>"] {
        if s.lines().any(|l| l.trim_start().starts_with(marker)) {
            return Err(format!(
                "edited hunk still has the `{marker}` conflict marker; \
                 remove the markers and one of the two sides, then save"
            ));
        }
    }
    Ok(())
}

/// Outcome of [`resolve_combined_file`] for a single sub-file within a
/// combined-hash entity. Distinguishes "resolved" outcomes (the caller
/// computes a fresh combined hash from the bytes on disk and records it
/// in the lockfile) from "preserve-base" outcomes (the user chose `[s]`
/// or hunk-walk `[s]`, so the lockfile entry for the *whole entity* must
/// not advance — the conflict has to re-surface on the next pull/sync).
#[derive(Debug)]
pub enum CombinedFileOutcome {
    /// Final bytes are on disk; caller may advance the combined hash.
    Resolved(Vec<u8>),
    /// Bytes are on disk (either kept-as-local for `[s]kip`, or marker-
    /// bearing for `[h] → [s]`), but the conflict is *not* resolved.
    /// The caller MUST preserve the prior lockfile base for this entity
    /// so the next pull/sync re-classifies it as a conflict.
    PreserveBase(Vec<u8>),
}

impl CombinedFileOutcome {
    /// Borrow the bytes (used by callers to feed the combined-hash
    /// helper) without consuming the outcome.
    pub fn bytes(&self) -> &[u8] {
        match self {
            CombinedFileOutcome::Resolved(b) | CombinedFileOutcome::PreserveBase(b) => b,
        }
    }
    /// Consume and return the bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            CombinedFileOutcome::Resolved(b) | CombinedFileOutcome::PreserveBase(b) => b,
        }
    }
    /// True when at least one sub-file in the entity asked the caller to
    /// preserve the prior lockfile base.
    pub fn is_preserve_base(&self) -> bool {
        matches!(self, CombinedFileOutcome::PreserveBase(_))
    }
}

/// Resolve a single sub-file within a combined-hash entity (hook
/// `.json`/`.py`, schema `schema.json`/formulas/`<id>.py`). Spec §8.3.
///
/// The caller passes the in-memory bytes for both sides. Behavior:
///
/// - `local_bytes == remote_bytes` → no-op (no prompt, no write); returns
///   `Resolved(local_bytes)`.
/// - `interactive == false` → legacy shadow-file: writes
///   `<local_path>.<env>`, keeps local on disk, returns
///   `PreserveBase(local_bytes)` — the caller must NOT advance the
///   combined-hash lockfile entry because the conflict is unresolved.
/// - `interactive == true && bytes differ` → prompt the user via
///   [`prompt_resolve`] with `[label_index/label_total]`. On `[k]eep`,
///   `[r]emote`, `[e]dit`: returns `Resolved(bytes)`. On `[s]kip` or
///   hunk-walk `[s]`: returns `PreserveBase(bytes)`. On `[a]bort`:
///   propagates [`PullAborted`].
///
/// The returned bytes are what now sits on disk for `local_path` (so the
/// caller can include them when computing the entity's combined hash if
/// it chooses to advance the lockfile).
pub fn resolve_combined_file(
    label_index: usize,
    label_total: usize,
    local_path: &Path,
    local_bytes: &[u8],
    remote_bytes: &[u8],
    interactive: bool,
    env: &str,
) -> Result<CombinedFileOutcome> {
    use crate::snapshot::writer::write_atomic;

    if local_bytes == remote_bytes {
        return Ok(CombinedFileOutcome::Resolved(local_bytes.to_vec()));
    }

    if !interactive {
        let conflict_path = crate::paths::shadow_path_for(local_path, env);
        write_atomic(&conflict_path, remote_bytes)?;
        let log = crate::log::Log::new(detect_color_mode(false));
        log.event(
            crate::log::Action::Warn,
            &format!(
                "{} conflict: local preserved, remote at {} (lockfile base preserved; re-run to resolve)",
                local_path.display(),
                conflict_path.display(),
            ),
        );
        return Ok(CombinedFileOutcome::PreserveBase(local_bytes.to_vec()));
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
        Resolution::KeepLocal => Ok(CombinedFileOutcome::Resolved(local_bytes.to_vec())),
        Resolution::KeepRemote => {
            write_atomic(local_path, remote_bytes)?;
            Ok(CombinedFileOutcome::Resolved(remote_bytes.to_vec()))
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
            Ok(CombinedFileOutcome::Resolved(edited))
        }
        Resolution::EditWithMarkers(edited) => {
            // User explicitly chose `[h]unk-by-hunk → [s]kip` on at
            // least one hunk; the bytes contain unresolved markers by
            // design. Skip the marker-leakage check but still validate
            // UTF-8 so the lockfile hash is over decodable bytes. The
            // marker-bearing content is intentionally on disk, but the
            // lockfile MUST NOT advance — the conflict needs to keep
            // re-surfacing until the user fully resolves it.
            if std::str::from_utf8(&edited).is_err() {
                anyhow::bail!(
                    "refusing to overwrite {} with non-UTF-8 hunk-walk result; local file left untouched",
                    local_path.display(),
                );
            }
            write_atomic(local_path, &edited)?;
            Ok(CombinedFileOutcome::PreserveBase(edited))
        }
        Resolution::Skip => {
            let conflict_path = crate::paths::shadow_path_for(local_path, env);
            write_atomic(&conflict_path, remote_bytes)?;
            let log = crate::log::Log::new(detect_color_mode(false));
            log.event(
                crate::log::Action::Warn,
                &format!(
                    "{} conflict: local preserved, remote at {} (lockfile base preserved; re-run to resolve)",
                    local_path.display(),
                    conflict_path.display(),
                ),
            );
            Ok(CombinedFileOutcome::PreserveBase(local_bytes.to_vec()))
        }
        Resolution::Abort => Err(anyhow::Error::new(PullAborted)),
    }
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
        // Hunk-walk-with-skipped-markers is meaningful on a pull (local
        // file ends up with markers, lockfile records the hash of those
        // bytes), but on push the override would be PATCHed straight to
        // the API and the server would reject the marker text. Force a
        // Skip so the local file stays put; the user can re-run with a
        // cleaner resolution.
        Resolution::EditWithMarkers(_) => Ok(PushDriftOutcome::Skip),
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
            // Two real users can share first+last name; the email is
            // the only field guaranteed unique per Rossum user. Render
            // it next to the display name (`<email>`, git-author style)
            // when present and not already what `display` collapsed to
            // (system accounts often have email == username, no point
            // showing the same string twice).
            let email_suffix = u
                .email
                .as_deref()
                .filter(|e| !e.is_empty() && *e != display)
                .map(|e| format!(" <{e}>"))
                .unwrap_or_default();
            format!("{display}{email_suffix}   [{tags}]   {}", u.url)
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

/// Cure choice for an anomalous store-extension hook. `Convert` is
/// the safe default (one PATCH, hook id preserved); `Reinstall` is
/// the heavier option (new id, dependents rewired); `Skip` leaves
/// it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnomalyCure {
    Convert,
    Reinstall,
    Skip,
}

/// Per-hook interactive prompt. Non-TTY → `Convert` is the default,
/// unless `RDC_DOCTOR_CURE` env var selects another option:
/// `"reinstall"` → Reinstall, `"skip"` → Skip. Anything else → Convert.
pub fn prompt_anomaly_cure(
    slug: &str,
    hook: &crate::model::Hook,
    interactive: bool,
) -> anyhow::Result<AnomalyCure> {
    if !interactive {
        let env_choice = std::env::var("RDC_DOCTOR_CURE").unwrap_or_default();
        return Ok(match env_choice.as_str() {
            "reinstall" => AnomalyCure::Reinstall,
            "skip" => AnomalyCure::Skip,
            _ => AnomalyCure::Convert,
        });
    }
    // TTY mode: use the project's existing prompt library (`inquire`,
    // matching `prompt_token_owner` above). Show config.private and
    // has-code signals so the operator can decide.
    let private = hook.config.get("private").and_then(|v| v.as_bool()).unwrap_or(false);
    let has_code = hook
        .config
        .get("code")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let prompt = format!(
        "Cure for hooks/{slug} (id {}, name {:?}, type {}, config.private={private}, has config.code={has_code})?",
        hook.id, hook.name, hook.hook_type
    );
    let options = vec![
        "[c] Convert to custom (one PATCH, id preserved)",
        "[r] Reinstall as store extension (new id, rewires dependents)",
        "[s] Skip this hook",
    ];
    use inquire::error::InquireError;
    // Ctrl-C / Esc → Skip (not error). The caller's loop persists the
    // lockfile after each successful cure, so mapping cancellation to
    // Skip lets the operator abort mid-flight without losing
    // bookkeeping for hooks already fixed — the current hook is left
    // alone and the loop exits naturally on the next iteration if the
    // user continues to cancel. Matches the cancel-handling pattern in
    // `prompt_token_owner`, adapted from `Option<...>` to this fn's
    // `Result<AnomalyCure>` return shape.
    let answer = match inquire::Select::new(&prompt, options).raw_prompt() {
        Ok(opt) => opt,
        Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => {
            return Ok(AnomalyCure::Skip);
        }
        Err(e) => return Err(anyhow::anyhow!("anomaly cure prompt: {e}")),
    };
    Ok(match answer.index {
        0 => AnomalyCure::Convert,
        1 => AnomalyCure::Reinstall,
        _ => AnomalyCure::Skip,
    })
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
const SGR_DIM: &str = "\x1b[2m";

// --- Styled diff renderer (line numbers, ± row backgrounds, JSON highlight) ---
//
// Row backgrounds use truecolor + the EL trick (`\x1b[K`): once a background
// is active, erase-to-end-of-line fills the rest of the row with it, so a
// removed/added row tints edge-to-edge regardless of content width.
// Foreground tokens inside a tinted row end with `\x1b[39m` (reset fg, keep
// bg) — never a full `\x1b[0m` — so the bg survives until the trailing
// EL + reset.
const SGR_BG_ADD: &str = "\x1b[48;2;20;48;28m"; // deep green (added row)
const SGR_BG_REMOVE: &str = "\x1b[48;2;60;24;26m"; // deep red (removed row)
const SGR_BG_ADD_HI: &str = "\x1b[48;2;38;92;52m"; // brighter green — changed span
const SGR_BG_REMOVE_HI: &str = "\x1b[48;2;120;42;46m"; // brighter red — changed span
const SGR_GUTTER: &str = "\x1b[38;2;120;120;120m"; // gray line numbers (context)
const SGR_GUTTER_ADD: &str = "\x1b[38;2;135;190;120m"; // green line number (added)
const SGR_GUTTER_REMOVE: &str = "\x1b[38;2;225;130;130m"; // red line number (removed)
const SGR_FG_DEFAULT: &str = "\x1b[39m"; // reset fg, preserve bg
const SGR_EOL: &str = "\x1b[K"; // erase to EOL → fills current bg
const SGR_J_KEY: &str = "\x1b[38;2;126;167;255m"; // JSON keys
const SGR_J_STR: &str = "\x1b[38;2;152;195;121m"; // JSON string values
const SGR_J_NUM: &str = "\x1b[38;2;229;181;103m"; // JSON numbers
const SGR_J_KW: &str = "\x1b[38;2;198;146;233m"; // true / false / null

/// Render a styled diff for inline display — the single renderer behind every
/// user-facing diff (`rdc diff`, dry-run/deploy previews, and the conflict
/// resolver), so they all share one look.
///
/// Layout: a `Verb(path)` header, an `Added N / removed M` summary, then
/// line-numbered hunks (3 lines of context) with gray gutters, red/green row
/// backgrounds on `-`/`+` lines, and simple JSON syntax highlighting (only
/// when `path` ends in `.json`). `Verb` is `Create` (left empty) / `Delete`
/// (right empty) / `Update`. In [`ColorMode::Plain`] the same layout renders
/// with no SGR at all (line numbers + `-`/`+` markers). Returns `""` when the
/// two sides are byte-identical.
pub fn render_styled_diff(
    left_label: &str,
    right_label: &str,
    left: &str,
    right: &str,
    mode: ColorMode,
) -> String {
    use similar::ChangeTag;
    use std::fmt::Write as _;

    let path = diff_display_path(left_label, right_label);
    let left_ann = label_annotation(left_label);
    let right_ann = label_annotation(right_label);
    let diff = line_diff(left, right);
    let groups = diff.grouped_ops(3);
    if groups.is_empty() {
        return String::new();
    }

    let (mut added, mut removed, mut max_line) = (0usize, 0usize, 1usize);
    for op in groups.iter().flatten() {
        for ch in diff.iter_changes(op) {
            if let Some(i) = ch.old_index() {
                max_line = max_line.max(i + 1);
            }
            if let Some(i) = ch.new_index() {
                max_line = max_line.max(i + 1);
            }
            match ch.tag() {
                ChangeTag::Insert => added += 1,
                ChangeTag::Delete => removed += 1,
                ChangeTag::Equal => {}
            }
        }
    }

    let w = max_line.to_string().len().max(3);
    let verb = if left.is_empty() {
        "Create"
    } else if right.is_empty() {
        "Delete"
    } else {
        "Update"
    };
    let is_json = path.ends_with(".json");
    let plain = mode == ColorMode::Plain;
    let s = |n: usize| if n == 1 { "" } else { "s" };

    let mut out = String::new();
    if plain {
        let _ = writeln!(out, "{verb}({path})");
        let _ = writeln!(
            out,
            "  Added {added} line{}, removed {removed} line{}",
            s(added),
            s(removed)
        );
    } else {
        let _ = writeln!(out, "{SGR_AMBER_BOLD}{verb}{SGR_RESET}({path})");
        let _ = writeln!(
            out,
            "  {SGR_DIM}\u{23bf} Added {added} line{}, removed {removed} line{}{SGR_RESET}",
            s(added),
            s(removed)
        );
    }

    // Side legend (when both labels carry a ` (…)` annotation) so the reader
    // knows what `-` and `+` mean — e.g. `- local  + remote`, or for a deploy
    // preview `- src after overlay+rewrite  + tgt remote`. The `-`/`+` tokens
    // are colored bold red/green to mirror the `-`/`+` row colors below, so the
    // side mapping reads at a glance — do not dim it back.
    if let (Some(la), Some(ra)) = (left_ann, right_ann) {
        let _ = if plain {
            writeln!(out, "  - {la}   + {ra}")
        } else {
            writeln!(
                out,
                "  {SGR_REMOVE_BOLD}- {la}{SGR_RESET}   {SGR_ADD_BOLD}+ {ra}{SGR_RESET}"
            )
        };
    }

    for (gi, group) in groups.iter().enumerate() {
        if gi > 0 {
            let _ = if plain {
                writeln!(out, "  \u{22ee}")
            } else {
                writeln!(out, "  {SGR_DIM}\u{22ee}{SGR_RESET}")
            };
        }
        for op in group {
            for change in diff.iter_inline_changes(op) {
                // Reassemble the row text and record which byte ranges differ
                // from the paired row (emphasized), for intra-line highlight.
                let mut content = String::new();
                let mut emph: Vec<(usize, usize)> = Vec::new();
                for (emphasized, val) in change.iter_strings_lossy() {
                    let start = content.len();
                    content.push_str(&val);
                    if emphasized {
                        emph.push((start, content.len()));
                    }
                }
                if content.ends_with('\n') {
                    content.pop();
                }
                let clen = content.len();
                for r in emph.iter_mut() {
                    r.0 = r.0.min(clen);
                    r.1 = r.1.min(clen);
                }
                emph.retain(|(s, e)| s < e);

                let (marker, idx) = match change.tag() {
                    ChangeTag::Equal => (' ', change.new_index()),
                    ChangeTag::Delete => ('-', change.old_index()),
                    ChangeTag::Insert => ('+', change.new_index()),
                };
                let n = idx.map(|i| i + 1).unwrap_or(0);

                if plain {
                    let _ = writeln!(out, "  {n:>w$} {marker} {content}");
                    continue;
                }
                let _ = match change.tag() {
                    ChangeTag::Delete => {
                        let body = render_content(&content, is_json, Some(SGR_BG_REMOVE), SGR_BG_REMOVE_HI, &emph);
                        writeln!(out, "{SGR_BG_REMOVE}  {SGR_GUTTER_REMOVE}{n:>w$} {marker}{SGR_FG_DEFAULT} {body}{SGR_EOL}{SGR_RESET}")
                    }
                    ChangeTag::Insert => {
                        let body = render_content(&content, is_json, Some(SGR_BG_ADD), SGR_BG_ADD_HI, &emph);
                        writeln!(out, "{SGR_BG_ADD}  {SGR_GUTTER_ADD}{n:>w$} {marker}{SGR_FG_DEFAULT} {body}{SGR_EOL}{SGR_RESET}")
                    }
                    ChangeTag::Equal => {
                        let body = render_content(&content, is_json, None, "", &[]);
                        writeln!(out, "  {SGR_GUTTER}{n:>w$}{SGR_RESET}   {body}")
                    }
                };
            }
        }
    }
    out
}

/// Derive the header path for [`render_styled_diff`] from the two side
/// labels callers pass (e.g. `"queues/q/queue.json (local)"` and
/// `"… (remote)"`, or `"/dev/null"` for a created/deleted side). Picks the
/// non-`/dev/null` side and strips a trailing ` (…)` annotation.
pub fn diff_display_path(a: &str, b: &str) -> String {
    let pick = if a == "/dev/null" { b } else { a };
    pick.rsplit_once(" (").map(|(p, _)| p).unwrap_or(pick).to_string()
}

/// Extract the ` (…)` annotation a caller appends to a diff side label
/// (e.g. `"hooks/x.json (src after overlay+rewrite)"` → `"src after
/// overlay+rewrite"`). Returns `None` for bare paths or `/dev/null`.
fn label_annotation(label: &str) -> Option<&str> {
    let start = label.rfind(" (")? + 2;
    label[start..].strip_suffix(')')
}

/// JSON syntax-highlight spans for one line: `(start, end, fg)` byte ranges
/// for `"keys"` (a string immediately followed by `:`), `"string values"`,
/// numbers, and the literals `true`/`false`/`null`. Bytes not covered by any
/// span render in the default foreground. Best-effort and line-local; never
/// panics.
fn json_fg_spans(line: &str) -> Vec<(usize, usize, &'static str)> {
    let b = line.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            let start = i;
            i += 1;
            while i < b.len() {
                match b[i] {
                    b'\\' => i = (i + 2).min(b.len()),
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            let mut j = i;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            let color = if j < b.len() && b[j] == b':' { SGR_J_KEY } else { SGR_J_STR };
            spans.push((start, i, color));
        } else if c.is_ascii_digit() || (c == b'-' && i + 1 < b.len() && b[i + 1].is_ascii_digit()) {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_digit() || matches!(b[i], b'.' | b'e' | b'E' | b'+' | b'-')) {
                i += 1;
            }
            spans.push((start, i, SGR_J_NUM));
        } else if let Some(kw) = ["true", "false", "null"]
            .into_iter()
            .find(|kw| line[i..].starts_with(kw))
            .filter(|kw| {
                let e = i + kw.len();
                e >= b.len() || !(b[e].is_ascii_alphanumeric() || b[e] == b'_')
            })
        {
            spans.push((i, i + kw.len(), SGR_J_KW));
            i += kw.len();
        } else {
            let ch = line[i..].chars().next().unwrap();
            i += ch.len_utf8();
        }
    }
    spans
}

/// Render one diff row's content (everything after the gutter + marker),
/// combining two overlays: JSON syntax highlighting (foreground) and
/// intra-line change emphasis — a brighter background (`hi_bg`) over the
/// `emph` byte ranges, which are the substrings that actually differ from the
/// paired row. `base_bg` is `Some` for changed (`-`/`+`) rows (the row's base
/// background, which the caller has already set and whose trailing `\x1b[K`
/// fills the rest of the line) and `None` for context rows. Foreground
/// changes use `\x1b[39m` so the active background is never disturbed; the
/// background is restored to `base_bg` before returning.
fn render_content(
    content: &str,
    is_json: bool,
    base_bg: Option<&str>,
    hi_bg: &str,
    emph: &[(usize, usize)],
) -> String {
    let fg = if is_json { json_fg_spans(content) } else { Vec::new() };
    let mut bounds: Vec<usize> = vec![0, content.len()];
    for (s, e, _) in &fg {
        bounds.push(*s);
        bounds.push(*e);
    }
    for (s, e) in emph {
        bounds.push(*s);
        bounds.push(*e);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut out = String::new();
    let mut cur_fg: Option<&str> = None;
    let mut cur_emph = false;
    for win in bounds.windows(2) {
        let (a, z) = (win[0], win[1]);
        if a >= z {
            continue;
        }
        let seg_fg = fg.iter().find(|(s, e, _)| *s <= a && a < *e).map(|(_, _, c)| *c);
        let seg_emph = emph.iter().any(|(s, e)| *s <= a && a < *e);
        if let Some(bg) = base_bg
            && seg_emph != cur_emph {
                out.push_str(if seg_emph { hi_bg } else { bg });
                cur_emph = seg_emph;
            }
        if seg_fg != cur_fg {
            out.push_str(seg_fg.unwrap_or(SGR_FG_DEFAULT));
            cur_fg = seg_fg;
        }
        out.push_str(&content[a..z]);
    }
    if cur_fg.is_some() {
        out.push_str(SGR_FG_DEFAULT);
    }
    if let Some(bg) = base_bg
        && cur_emph {
            out.push_str(bg);
        }
    out
}

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

/// Colorize a success line (`✓ <name>`) in sage green — matches the
/// hue used for added lines in unified diff output.
pub fn colorize_success(text: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return text.to_string();
    }
    format!("{SGR_ADD}{text}{SGR_RESET}")
}

/// Colorize a warning line (`⚠️ <name>`) in bold amber — matches the
/// primary accent used by clap headers and prompt brackets.
pub fn colorize_warning(text: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return text.to_string();
    }
    format!("{SGR_AMBER_BOLD}{text}{SGR_RESET}")
}

/// Colorize an error line (`✗ <name>`) in bold red — matches the
/// hue used for removed file headers in unified diff output.
pub fn colorize_error(text: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return text.to_string();
    }
    format!("{SGR_REMOVE_BOLD}{text}{SGR_RESET}")
}

/// Colorize the final summary line (`✔ Synced …`) in bold sage green.
pub fn colorize_final_ok(text: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return text.to_string();
    }
    format!("{SGR_ADD_BOLD}{text}{SGR_RESET}")
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
            if let Some(&letter) = chars.peek()
                && letter.is_ascii_alphabetic() {
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
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Colorize text in dim/faint style. Used for the time prefix and
/// low-importance action tokens (`skip`, `info`, `tick`, `idle`).
pub fn colorize_dim(text: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return text.to_string();
    }
    format!("{SGR_DIM}{text}{SGR_RESET}")
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
    fn build_conflict_buffer_marks_only_differing_hunks() {
        let local = b"a\nb\nc\nd\ne\n";
        let remote = b"a\nb\nXXX\nd\ne\n";
        let buf = build_conflict_buffer(local, remote, "production");
        let s = String::from_utf8(buf).unwrap();

        // The equal prefix and suffix should be present without markers.
        assert!(s.contains("a\nb\n"), "equal prefix missing: {s}");
        assert!(s.contains("d\ne\n"), "equal suffix missing: {s}");
        // Only the differing hunk should be wrapped.
        assert!(
            s.contains("<<<<<<< local\nc\n=======\nXXX\n>>>>>>> production\n"),
            "{s}"
        );
        // The whole-file form should NOT appear.
        assert!(
            !s.contains("<<<<<<< local\na\nb\nc\nd\ne\n"),
            "whole-file marker leaked: {s}"
        );
    }

    #[test]
    fn build_conflict_buffer_identical_files_produces_no_markers() {
        let local = b"a\nb\nc\n";
        let remote = b"a\nb\nc\n";
        let buf = build_conflict_buffer(local, remote, "production");
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains("<<<<<<<"), "no markers expected: {s}");
        assert!(!s.contains("======="), "no markers expected: {s}");
        assert!(!s.contains(">>>>>>>"), "no markers expected: {s}");
        assert_eq!(s, "a\nb\nc\n");
    }

    #[test]
    fn build_conflict_buffer_uses_env_name_in_marker() {
        let local = b"x\n";
        let remote = b"y\n";
        let buf = build_conflict_buffer(local, remote, "staging");
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains(">>>>>>> staging\n"), "{s}");
        assert!(
            !s.contains(">>>>>>> remote"),
            "literal 'remote' should not appear: {s}"
        );
    }

    #[test]
    fn build_conflict_buffer_handles_multiple_hunks() {
        let local = b"a\nFOO\nb\nBAR\nc\n";
        let remote = b"a\nfoo\nb\nbar\nc\n";
        let buf = build_conflict_buffer(local, remote, "production");
        let s = String::from_utf8(buf).unwrap();
        // Two separate marker blocks.
        let marker_count = s.matches("<<<<<<< local").count();
        assert_eq!(marker_count, 2, "expected 2 conflict blocks, got {marker_count}: {s}");
    }

    #[test]
    fn build_conflict_buffer_empty_local_emits_remote_only_block() {
        let local = b"";
        let remote = b"x\ny\n";
        let buf = build_conflict_buffer(local, remote, "production");
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("<<<<<<< local\n=======\nx\ny\n>>>>>>> production\n"),
            "{s}"
        );
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
        assert_eq!(out.bytes(), b"same\n");
        // Bytes-equal sides are a "Resolved" outcome — caller may advance.
        assert!(!out.is_preserve_base(), "equal bytes must not preserve base");
        // No shadow file written.
        assert!(!dir.path().join("a.py.test").exists());
    }

    #[test]
    fn resolve_combined_file_writes_shadow_when_non_interactive() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.py");
        std::fs::write(&path, b"local\n").unwrap();
        let out = resolve_combined_file(1, 1, &path, b"local\n", b"remote\n", false, "test").unwrap();
        assert_eq!(out.bytes(), b"local\n");
        // Non-interactive shadow-skip MUST signal preserve-base so the
        // caller does not advance the entity's combined hash.
        assert!(
            out.is_preserve_base(),
            "non-interactive shadow-fallback must signal preserve-base"
        );
        assert_eq!(std::fs::read(dir.path().join("a.py.test")).unwrap(), b"remote\n");
        // Local file untouched.
        assert_eq!(std::fs::read(&path).unwrap(), b"local\n");
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
        // Conflict header + action letters in bold amber; the styled diff
        // marks changed rows with red/green backgrounds.
        assert!(s.contains(SGR_AMBER_BOLD), "no amber accent: {s:?}");
        assert!(
            s.contains(SGR_BG_REMOVE) || s.contains(SGR_BG_ADD),
            "no diff row background: {s:?}"
        );
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
    fn picker_includes_email_for_disambiguation() {
        // Two real users can share first+last name; email is the unique
        // identifier the operator can use to tell them apart.
        use crate::model::User;
        let users: Vec<User> = serde_json::from_value(serde_json::json!([
            {"id": 100, "url": "u100", "username": "alice@a.com",
             "email": "alice@a.com",
             "first_name": "Alice", "last_name": "Smith",
             "is_active": true, "groups": ["https://x/groups/3"]},
            {"id": 200, "url": "u200", "username": "alice@b.com",
             "email": "alice@b.com",
             "first_name": "Alice", "last_name": "Smith",
             "is_active": true, "groups": ["https://x/groups/3"]}
        ])).unwrap();
        let choices = format_user_choices(&users, None);
        // Each line carries its own email so the operator can pick.
        assert!(
            choices.iter().any(|c| c.contains("Alice Smith <alice@a.com>")),
            "expected '<alice@a.com>' in some line, got {:?}",
            choices,
        );
        assert!(
            choices.iter().any(|c| c.contains("Alice Smith <alice@b.com>")),
            "expected '<alice@b.com>' in some line, got {:?}",
            choices,
        );
    }

    #[test]
    fn picker_omits_email_when_absent_or_equal_to_display() {
        // System users typically have no separate email (just the
        // synthetic `system_user__<hash>` username). Don't render an
        // empty `<>` or a redundant duplicate.
        use crate::model::User;
        let users: Vec<User> = serde_json::from_value(serde_json::json!([
            {"id": 1, "url": "u1", "username": "system_user__abc",
             "first_name": "SYS", "last_name": "USER",
             "is_active": true, "groups": ["https://x/groups/3"]},
            {"id": 2, "url": "u2", "username": "name-as-username",
             "email": "name-as-username",
             "first_name": "", "last_name": "",
             "is_active": true, "groups": ["https://x/groups/3"]}
        ])).unwrap();
        let choices = format_user_choices(&users, None);
        for c in &choices {
            assert!(!c.contains("<>"), "no empty email markers: {:?}", c);
        }
        // For the username-only user, display == email; suppress the
        // duplicate.
        let same = choices.iter().find(|c| c.contains("name-as-username")).unwrap();
        assert!(
            !same.contains("<name-as-username>"),
            "should suppress redundant `<email>` when it equals display, got {:?}",
            same,
        );
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
        // The env name now lives only in the prompt's `[r]` label, not the
        // diff body (the styled renderer headers with the file path). Confirm
        // the styled diff rendered and shows the changed value.
        assert!(s.contains("Update("), "styled diff header missing: {s}");
        assert!(s.contains("\"a\": 2"), "diff should show the remote value: {s}");
        assert!(!s.contains("[r]emote"), "old literal label leaked: {s}");
    }

    #[test]
    fn render_styled_diff_plain_layout() {
        let l = "{\n  \"hidden\": true\n}\n";
        let r = "{\n  \"hidden\": false\n}\n";
        let out = render_styled_diff("q/schema.json (local)", "q/schema.json (remote)", l, r, ColorMode::Plain);
        assert!(out.starts_with("Update(q/schema.json)\n"), "header: {out}");
        assert!(out.contains("Added 1 line, removed 1 line"), "summary: {out}");
        assert!(out.contains("- local   + remote"), "side legend: {out}");
        assert!(out.contains(" - "), "removed marker: {out}");
        assert!(out.contains(" + "), "added marker: {out}");
        assert!(out.contains("true") && out.contains("false"), "both values: {out}");
        assert!(!out.contains('\u{1b}'), "plain mode must carry no SGR: {out}");
        // identical sides → empty
        assert!(render_styled_diff("q/x.json (local)", "q/x.json (remote)", l, l, ColorMode::Plain).is_empty());
        // verb reflects one-sided diffs
        assert!(render_styled_diff("/dev/null", "q/x.json", "", r, ColorMode::Plain).starts_with("Create("));
        assert!(render_styled_diff("q/x.json", "/dev/null", l, "", ColorMode::Plain).starts_with("Delete("));
    }

    #[test]
    fn render_styled_diff_color_backgrounds_and_highlight() {
        let l = "{\n  \"hidden\": true\n}\n";
        let r = "{\n  \"hidden\": false\n}\n";
        let out = render_styled_diff("q/schema.json (local)", "q/schema.json (remote)", l, r, ColorMode::Color);
        assert!(!out.contains('\u{25cf}'), "decorative header bullet should be removed: {out:?}");
        assert!(out.contains(SGR_BG_REMOVE), "removed row needs a red background");
        assert!(out.contains(SGR_BG_ADD), "added row needs a green background");
        assert!(out.contains(SGR_EOL), "rows must fill the background to the line end");
        assert!(out.contains(SGR_J_KEY), "JSON keys should be highlighted");
        assert!(out.contains(SGR_J_KW), "true/false should be highlighted");
        // Colored gutters on changed rows.
        assert!(out.contains(SGR_GUTTER_REMOVE), "removed line number should be red");
        assert!(out.contains(SGR_GUTTER_ADD), "added line number should be green");
        // Intra-line emphasis: the changed value carries the brighter bg.
        assert!(out.contains(SGR_BG_REMOVE_HI), "changed span on removed row needs brighter red");
        assert!(out.contains(SGR_BG_ADD_HI), "changed span on added row needs brighter green");
        // Side legend tokens are color-coded (bold red/green) to mirror the
        // -/+ rows, not dimmed.
        assert!(out.contains(&format!("{SGR_REMOVE_BOLD}- local")), "legend '- local' should be bold red: {out:?}");
        assert!(out.contains(&format!("{SGR_ADD_BOLD}+ remote")), "legend '+ remote' should be bold green: {out:?}");
        // Non-.json content is not JSON-highlighted (but still gets row bg).
        let py = render_styled_diff("hooks/h.py (local)", "hooks/h.py (remote)", "a = 1\n", "a = 2\n", ColorMode::Color);
        assert!(
            !py.contains(SGR_J_KEY) && !py.contains(SGR_J_NUM),
            "non-json must skip syntax highlighting: {py:?}"
        );
        assert!(py.contains(SGR_BG_ADD), "non-json still gets row backgrounds");
    }

    #[test]
    fn prompt_remote_delete_offers_restore_and_mirror_labels() {
        use std::io::Cursor;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("labels/audit-hold.json");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, b"{\"name\":\"Audit hold\"}").unwrap();

        let mut out: Vec<u8> = Vec::new();
        let input = Cursor::new(b"s\n");
        let res = prompt_remote_delete_with_color(
            input, &mut out, &local, "production", ColorMode::Plain,
        ).unwrap();
        assert!(matches!(res, Resolution::Skip));

        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("deleted on production"), "header: {s}");
        assert!(s.contains("[k] keep local (restore on production)"), "k label: {s}");
        assert!(s.contains("[r] use production (delete local)"), "r label: {s}");
        assert!(!s.contains("[e]"), "no edit option in delete prompt: {s}");
    }

    #[test]
    fn prompt_remote_delete_returns_keep_local_on_k() {
        use std::io::Cursor;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("x.json");
        std::fs::write(&local, b"{}").unwrap();
        let mut out: Vec<u8> = Vec::new();
        let input = Cursor::new(b"k\n");
        let res = prompt_remote_delete_with_color(
            input, &mut out, &local, "test", ColorMode::Plain,
        ).unwrap();
        assert!(matches!(res, Resolution::KeepLocal));
    }

    #[test]
    fn prompt_remote_delete_returns_keep_remote_on_r() {
        use std::io::Cursor;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("x.json");
        std::fs::write(&local, b"{}").unwrap();
        let mut out: Vec<u8> = Vec::new();
        let input = Cursor::new(b"r\n");
        let res = prompt_remote_delete_with_color(
            input, &mut out, &local, "test", ColorMode::Plain,
        ).unwrap();
        assert!(matches!(res, Resolution::KeepRemote));
    }

    #[test]
    fn prompt_remote_delete_returns_abort_on_a() {
        use std::io::Cursor;
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("x.json");
        std::fs::write(&local, b"{}").unwrap();
        let mut out: Vec<u8> = Vec::new();
        let input = Cursor::new(b"a\n");
        let res = prompt_remote_delete_with_color(
            input, &mut out, &local, "test", ColorMode::Plain,
        ).unwrap();
        assert!(matches!(res, Resolution::Abort));
    }

    /// Three differing hunks separated by equal context. Caller-friendly
    /// fixture used by all `prompt_hunk_by_hunk_*` tests.
    fn three_hunk_fixture() -> (Vec<u8>, Vec<u8>) {
        let local = b"a\nFOO\nb\nBAR\nc\nBAZ\nd\n".to_vec();
        let remote = b"a\nfoo\nb\nbar\nc\nbaz\nd\n".to_vec();
        (local, remote)
    }

    #[test]
    fn prompt_hunk_by_hunk_keep_all_local_yields_local_bytes() {
        let (local, remote) = three_hunk_fixture();
        let mut output: Vec<u8> = Vec::new();
        let mut input = Cursor::new(b"k\nk\nk\n".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "production", ColorMode::Plain,
        ).unwrap();
        match outcome {
            EditOutcome::Edited(bytes) => assert_eq!(bytes, local),
            other => panic!("expected Edited(local), got {other:?}"),
        }
    }

    #[test]
    fn prompt_hunk_by_hunk_use_all_remote_yields_remote_bytes() {
        let (local, remote) = three_hunk_fixture();
        let mut output: Vec<u8> = Vec::new();
        let mut input = Cursor::new(b"r\nr\nr\n".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "production", ColorMode::Plain,
        ).unwrap();
        match outcome {
            EditOutcome::Edited(bytes) => assert_eq!(bytes, remote),
            other => panic!("expected Edited(remote), got {other:?}"),
        }
    }

    #[test]
    fn prompt_hunk_by_hunk_mixed_decisions_yields_correct_merge() {
        let (local, remote) = three_hunk_fixture();
        let mut output: Vec<u8> = Vec::new();
        // keep, remote, keep
        let mut input = Cursor::new(b"k\nr\nk\n".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "production", ColorMode::Plain,
        ).unwrap();
        let bytes = match outcome {
            EditOutcome::Edited(b) => b,
            other => panic!("expected Edited, got {other:?}"),
        };
        // Hunk 1 = local (FOO), hunk 2 = remote (bar), hunk 3 = local (BAZ).
        // Equal lines (a, b, c, d) preserved.
        let expected = b"a\nFOO\nb\nbar\nc\nBAZ\nd\n".to_vec();
        assert_eq!(
            bytes,
            expected,
            "got {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    #[test]
    fn prompt_hunk_by_hunk_both_emits_local_then_remote() {
        // Single-hunk fixture so the only decision needed is one `b`.
        let local = b"a\nLOCAL\nb\n".to_vec();
        let remote = b"a\nREMOTE\nb\n".to_vec();
        let mut output: Vec<u8> = Vec::new();
        let mut input = Cursor::new(b"b\n".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "production", ColorMode::Plain,
        ).unwrap();
        let bytes = match outcome {
            EditOutcome::Edited(b) => b,
            other => panic!("expected Edited, got {other:?}"),
        };
        // Local lines first, then remote lines, no markers.
        let expected = b"a\nLOCAL\nREMOTE\nb\n".to_vec();
        assert_eq!(
            bytes,
            expected,
            "got {:?}",
            String::from_utf8_lossy(&bytes)
        );
        let s = String::from_utf8_lossy(&bytes);
        assert!(!s.contains("<<<<<<<"), "no markers on `[b]oth`: {s}");
    }

    #[test]
    fn prompt_hunk_by_hunk_skip_preserves_markers() {
        let local = b"a\nLOCAL\nb\n".to_vec();
        let remote = b"a\nREMOTE\nb\n".to_vec();
        let mut output: Vec<u8> = Vec::new();
        let mut input = Cursor::new(b"s\n".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "production", ColorMode::Plain,
        ).unwrap();
        let bytes = match outcome {
            EditOutcome::EditedWithMarkers(b) => b,
            other => panic!("expected EditedWithMarkers, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("<<<<<<< local\nLOCAL\n=======\nREMOTE\n>>>>>>> production\n"),
            "expected wrapped hunk: {s}"
        );
    }

    #[test]
    fn prompt_hunk_by_hunk_abort_returns_aborted() {
        let (local, remote) = three_hunk_fixture();
        let mut output: Vec<u8> = Vec::new();
        let mut input = Cursor::new(b"a\n".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "production", ColorMode::Plain,
        ).unwrap();
        assert!(matches!(outcome, EditOutcome::Aborted), "got {outcome:?}");
    }

    #[test]
    fn prompt_single_hunk_displays_surrounding_context() {
        // Local has 10 lines, hunk at line 4 (one line changed). The
        // display must render at least one line of equal context above
        // and below the differing block, following the unified-diff
        // convention (leading space).
        let local = b"line1\nline2\nline3\nLOCAL_LINE\nline5\nline6\nline7\nline8\nline9\nline10\n";
        let remote = b"line1\nline2\nline3\nREMOTE_LINE\nline5\nline6\nline7\nline8\nline9\nline10\n";

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.py");
        std::fs::write(&path, local).unwrap();

        let mut input = Cursor::new(b"s\n".to_vec());
        let mut output: Vec<u8> = Vec::new();
        let _ = prompt_hunk_by_hunk(
            &mut input,
            &mut output,
            local,
            remote,
            &path,
            "test",
            ColorMode::Plain,
        )
        .unwrap();

        let s = String::from_utf8_lossy(&output);
        // The differing lines themselves:
        assert!(s.contains("-LOCAL_LINE"), "diff should show local-removed: {s}");
        assert!(s.contains("+REMOTE_LINE"), "diff should show remote-added: {s}");
        // Surrounding context — at least one of the lines before / after
        // the hunk must appear with the leading-space prefix that the
        // unified-diff convention uses for equal lines.
        assert!(
            s.contains(" line1") || s.contains(" line2") || s.contains(" line3"),
            "should include lines preceding the hunk as context: {s}"
        );
        assert!(
            s.contains(" line5") || s.contains(" line6") || s.contains(" line7"),
            "should include lines following the hunk as context: {s}"
        );
    }

    #[test]
    fn prompt_resolve_shows_h_only_for_multi_hunk() {
        // Single-hunk case — `[h]` must not appear, header must not say "(N hunks)".
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.py");
        std::fs::write(&path, b"a\nLOCAL\nb\n").unwrap();
        let mut output: Vec<u8> = Vec::new();
        let input = Cursor::new(b"s\n");
        let _ = prompt_resolve_with_color(
            input,
            &mut output,
            1,
            1,
            &path,
            b"a\nREMOTE\nb\n",
            "production",
            ColorMode::Plain,
        ).unwrap();
        let s = String::from_utf8(output).unwrap();
        assert!(!s.contains("[h]"), "single-hunk prompt must not offer [h]: {s}");
        assert!(!s.contains("hunks)"), "single-hunk header must not advertise count: {s}");

        // Three-hunk case — `[h] hunk-by-hunk` must appear, header must say "(3 hunks)".
        let path3 = dir.path().join("y.py");
        std::fs::write(&path3, b"a\nFOO\nb\nBAR\nc\nBAZ\nd\n").unwrap();
        let mut output3: Vec<u8> = Vec::new();
        let input3 = Cursor::new(b"s\n");
        let _ = prompt_resolve_with_color(
            input3,
            &mut output3,
            1,
            1,
            &path3,
            b"a\nfoo\nb\nbar\nc\nbaz\nd\n",
            "production",
            ColorMode::Plain,
        ).unwrap();
        let s3 = String::from_utf8(output3).unwrap();
        assert!(
            s3.contains("[h] hunk-by-hunk"),
            "multi-hunk prompt must offer [h] hunk-by-hunk: {s3}"
        );
        assert!(
            s3.contains("(3 hunks)"),
            "multi-hunk header must advertise count: {s3}"
        );
    }

    // ---------------------------------------------------------------
    // Adversarial example tests + property tests.
    //
    // Group A targets `build_conflict_buffer` edge cases (markers in
    // content, empty sides, unicode, CR-LF, many hunks).
    // Group B exercises `prompt_hunk_by_hunk` under unusual inputs (EOF
    // mid-walk, unknown actions, immediate abort, mixed decisions,
    // identical inputs).
    // Group C pins behavior of the two validators on each `.json` /
    // `.py` path and across every marker/UTF-8/JSON failure mode.
    // Group D pins the top-level resolver UI rendering.
    // ---------------------------------------------------------------

    use proptest::prelude::*;

    /// Count non-Equal diff ops between two byte slices. Mirrors the
    /// counting `prompt_hunk_by_hunk` does internally so the property
    /// tests can pre-compute exactly how many decisions to feed on
    /// stdin.
    fn count_hunk_ops(local: &[u8], remote: &[u8]) -> usize {
        use similar::DiffTag;
        let local_str = String::from_utf8_lossy(local);
        let remote_str = String::from_utf8_lossy(remote);
        let diff = line_diff(local_str.as_ref(), remote_str.as_ref());
        diff.ops()
            .iter()
            .filter(|op| op.tag() != DiffTag::Equal)
            .count()
    }

    // === Invariant 1: identical inputs emit no markers ==========
    //
    // build_conflict_buffer always appends a defensive trailing `\n`
    // when the output is non-empty (so the editor buffer is properly
    // line-terminated). The "exactly equal" check therefore only holds
    // for inputs that already end in `\n` (or are empty). Production
    // callers normalize through `prettify_json_for_diff` first, which
    // always produces newline-terminated bytes.
    proptest! {
        #[test]
        fn build_conflict_buffer_identical_inputs_emit_no_markers(s in "[a-zA-Z0-9 \n]{0,500}") {
            let buf = build_conflict_buffer(s.as_bytes(), s.as_bytes(), "test");
            let out = String::from_utf8(buf).unwrap();
            prop_assert!(!out.contains("<<<<<<<"));
            prop_assert!(!out.contains("======="));
            prop_assert!(!out.contains(">>>>>>>"));
            // The buffer always equals the input modulo a possible
            // trailing-newline normalization.
            if s.is_empty() || s.ends_with('\n') {
                prop_assert_eq!(&out, &s);
            } else {
                prop_assert_eq!(out, format!("{s}\n"));
            }
        }
    }

    // === Invariant 2: differing inputs always emit a marker block ===
    proptest! {
        #[test]
        fn build_conflict_buffer_differing_inputs_emit_at_least_one_marker_block(
            a in "[a-zA-Z0-9 \n]{1,200}",
            b in "[a-zA-Z0-9 \n]{1,200}",
        ) {
            prop_assume!(a != b);
            let buf = build_conflict_buffer(a.as_bytes(), b.as_bytes(), "test");
            let out = String::from_utf8(buf).unwrap();
            prop_assert!(out.contains("<<<<<<< local"));
            prop_assert!(out.contains("======="));
            prop_assert!(out.contains(">>>>>>> test"));
        }
    }

    // === Invariant 3: validate_edited rejects any buffer output for differing inputs ===
    proptest! {
        #[test]
        fn validate_edited_always_rejects_buffer_output(
            a in "[a-zA-Z0-9 \n]{1,200}",
            b in "[a-zA-Z0-9 \n]{1,200}",
        ) {
            prop_assume!(a != b);
            let buf = build_conflict_buffer(a.as_bytes(), b.as_bytes(), "test");
            let result = validate_edited(&buf, std::path::Path::new("x.py"));
            prop_assert!(result.is_err(), "validate_edited should reject buffer with markers");
        }
    }

    // === Invariant 4: hunk walker, all-keep-local yields local ===
    //
    // prompt_hunk_by_hunk normalizes every line in its merged output to
    // end with `\n` (so subsequent lines never glue together). The
    // "exactly equal" check therefore requires the input to be
    // newline-terminated — which matches production callers, who only
    // ever pass pretty-printed JSON or .py files (both newline-terminated).
    // The regex forces a final `\n` so we don't waste cases on the
    // (separately tested) trailing-newline normalization branch.
    proptest! {
        #[test]
        fn hunk_by_hunk_all_keep_local_yields_local(
            local in "[a-zA-Z0-9 \n]{0,299}\n",
            remote in "[a-zA-Z0-9 \n]{0,299}\n",
        ) {
            prop_assume!(local != remote);
            let k = count_hunk_ops(local.as_bytes(), remote.as_bytes());
            prop_assume!(k > 0);

            let input_str: String = "k\n".repeat(k);
            let input = std::io::Cursor::new(input_str);
            let mut output: Vec<u8> = Vec::new();
            let outcome = prompt_hunk_by_hunk(
                &mut std::io::BufReader::new(input),
                &mut output,
                local.as_bytes(),
                remote.as_bytes(),
                std::path::Path::new("x.py"),
                "test",
                ColorMode::Plain,
            ).unwrap();
            match outcome {
                EditOutcome::Edited(bytes) => {
                    let result = String::from_utf8(bytes).unwrap();
                    prop_assert_eq!(&result, &local);
                }
                EditOutcome::EditedWithMarkers(_) =>
                    prop_assert!(false, "no [s] picked, should not have markers"),
                EditOutcome::Aborted => prop_assert!(false, "should not abort"),
            }
        }
    }

    // === Invariant 5: hunk walker, all-use-remote yields remote ===
    proptest! {
        #[test]
        fn hunk_by_hunk_all_use_remote_yields_remote(
            local in "[a-zA-Z0-9 \n]{0,299}\n",
            remote in "[a-zA-Z0-9 \n]{0,299}\n",
        ) {
            prop_assume!(local != remote);
            let k = count_hunk_ops(local.as_bytes(), remote.as_bytes());
            prop_assume!(k > 0);

            let input_str: String = "r\n".repeat(k);
            let input = std::io::Cursor::new(input_str);
            let mut output: Vec<u8> = Vec::new();
            let outcome = prompt_hunk_by_hunk(
                &mut std::io::BufReader::new(input),
                &mut output,
                local.as_bytes(),
                remote.as_bytes(),
                std::path::Path::new("x.py"),
                "test",
                ColorMode::Plain,
            ).unwrap();
            match outcome {
                EditOutcome::Edited(bytes) => {
                    let result = String::from_utf8(bytes).unwrap();
                    prop_assert_eq!(&result, &remote);
                }
                EditOutcome::EditedWithMarkers(_) =>
                    prop_assert!(false, "no [s] picked, should not have markers"),
                EditOutcome::Aborted => prop_assert!(false, "should not abort"),
            }
        }
    }

    // === Invariant 6: validate_markers_only accepts clean content ===
    proptest! {
        #[test]
        fn validate_markers_only_accepts_clean_content(s in "[a-zA-Z0-9 \n]{0,500}") {
            // Markers are caught even when indented (post-fix), so
            // filter out content where any line's trim-start begins
            // with one of the three marker tokens.
            prop_assume!(!s.lines().any(|l| {
                let t = l.trim_start();
                t.starts_with("<<<<<<<") || t.starts_with("=======") || t.starts_with(">>>>>>>")
            }));
            let result = validate_edited_markers_only(s.as_bytes());
            prop_assert!(result.is_ok(), "should accept marker-free content");
        }
    }

    // === Invariant 7: validate_markers_only rejects any marker on any line ===
    proptest! {
        #[test]
        fn validate_markers_only_rejects_any_marker(
            prefix in "[a-zA-Z0-9 \n]{0,200}",
            suffix in "[a-zA-Z0-9 \n]{0,200}",
            which in 0u8..3u8,
        ) {
            let marker = match which { 0 => "<<<<<<<", 1 => "=======", _ => ">>>>>>>" };
            let s = format!("{prefix}\n{marker} blah\n{suffix}");
            let result = validate_edited_markers_only(s.as_bytes());
            prop_assert!(result.is_err());
        }
    }

    // ============================================================
    // Group A — build_conflict_buffer adversarial examples.
    // ============================================================

    #[test]
    fn build_conflict_buffer_handles_literal_marker_in_identical_content() {
        // A line that *looks* like a marker is content, not a real marker;
        // when local == remote it must pass through unchanged.
        let s = b"<<<<<<< this looks like a marker but isn't\nfoo\n";
        let buf = build_conflict_buffer(s, s, "test");
        assert_eq!(
            buf,
            s.to_vec(),
            "identical content with marker-like line should pass through unchanged"
        );
    }

    #[test]
    fn build_conflict_buffer_handles_marker_in_local_diff() {
        // local has a marker-like banner; remote has a comment-banner.
        // Both share a "foo" context line. The marker line should appear
        // inside the local section of the conflict block. Note: this
        // produces a buffer where a marker-like line is INSIDE a
        // `<<<<<<< local` block — validate_edited will still reject it
        // because of the OUTER markers, which is correct behavior.
        let local = b"<<<<<<< banner\nfoo\n";
        let remote = b"# banner\nfoo\n";
        let buf = build_conflict_buffer(local, remote, "test");
        let s = String::from_utf8(buf).unwrap();
        // Outer markers present.
        assert!(s.contains("<<<<<<< local\n"), "{s}");
        assert!(s.contains(">>>>>>> test\n"), "{s}");
        // Equal context preserved.
        assert!(s.contains("foo\n"), "{s}");
        // validate_edited rejects this — correct, because the OUTER
        // markers are still there.
        let err = validate_edited(s.as_bytes(), std::path::Path::new("x.py")).unwrap_err();
        assert!(err.contains("conflict marker"), "got: {err}");
    }

    #[test]
    fn build_conflict_buffer_handles_empty_local() {
        let local: &[u8] = b"";
        let remote: &[u8] = b"a\nb\n";
        let buf = build_conflict_buffer(local, remote, "test");
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "<<<<<<< local\n=======\na\nb\n>>>>>>> test\n", "{s:?}");
    }

    #[test]
    fn build_conflict_buffer_handles_empty_remote() {
        let local: &[u8] = b"a\nb\n";
        let remote: &[u8] = b"";
        let buf = build_conflict_buffer(local, remote, "test");
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "<<<<<<< local\na\nb\n=======\n>>>>>>> test\n", "{s:?}");
    }

    #[test]
    fn build_conflict_buffer_handles_both_empty() {
        let buf = build_conflict_buffer(b"", b"", "test");
        assert!(buf.is_empty(), "both empty should emit no output: {buf:?}");
    }

    #[test]
    fn build_conflict_buffer_handles_single_line_no_trailing_newline() {
        let local = b"a";
        let remote = b"b";
        let buf = build_conflict_buffer(local, remote, "test");
        let s = String::from_utf8(buf).unwrap();
        // Marker block must be produced; the no-trailing-newline lines
        // are normalized with synthetic newlines.
        assert!(s.contains("<<<<<<< local\n"), "{s:?}");
        assert!(s.contains("a\n"), "{s:?}");
        assert!(s.contains("=======\n"), "{s:?}");
        assert!(s.contains("b\n"), "{s:?}");
        assert!(s.contains(">>>>>>> test\n"), "{s:?}");
        // Defensive trailing-newline: the output must always end with
        // a newline (or be empty).
        assert!(s.ends_with('\n'), "{s:?}");
    }

    #[test]
    fn build_conflict_buffer_handles_crlf_line_endings() {
        // similar treats CR-LF as part of the line. The diff still works,
        // and the marker block is produced for the differing line.
        let local = b"a\r\nb\r\n";
        let remote = b"a\r\nc\r\n";
        let buf = build_conflict_buffer(local, remote, "test");
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("<<<<<<< local\n"), "{s:?}");
        assert!(s.contains("b\r\n"), "{s:?}");
        assert!(s.contains("c\r\n"), "{s:?}");
        assert!(s.contains(">>>>>>> test\n"), "{s:?}");
        // The equal prefix "a\r\n" is preserved verbatim.
        assert!(s.starts_with("a\r\n"), "{s:?}");
    }

    #[test]
    fn build_conflict_buffer_handles_unicode_content() {
        let local = "こんにちは\n世界\n".as_bytes();
        let remote = "こんばんは\n世界\n".as_bytes();
        let buf = build_conflict_buffer(local, remote, "test");
        let s = String::from_utf8(buf).unwrap();
        // Only the first line differs; the second is equal and must
        // sit outside any marker block.
        assert!(s.contains("<<<<<<< local\nこんにちは\n"), "{s}");
        assert!(s.contains("=======\nこんばんは\n"), "{s}");
        assert!(s.contains(">>>>>>> test\n世界\n"), "{s}");
    }

    #[test]
    fn build_conflict_buffer_handles_all_different() {
        // No shared lines — one giant hunk.
        let local = b"a\nb\nc\n";
        let remote = b"x\ny\nz\n";
        let buf = build_conflict_buffer(local, remote, "test");
        let s = String::from_utf8(buf).unwrap();
        let marker_count = s.matches("<<<<<<< local").count();
        assert_eq!(marker_count, 1, "expected 1 block, got {marker_count}: {s}");
        assert!(s.contains("<<<<<<< local\na\nb\nc\n=======\nx\ny\nz\n>>>>>>> test\n"), "{s}");
    }

    #[test]
    fn build_conflict_buffer_handles_many_tiny_hunks() {
        let local = b"a\nFOO\nb\nBAR\nc\nBAZ\n";
        let remote = b"a\nfoo\nb\nbar\nc\nbaz\n";
        let buf = build_conflict_buffer(local, remote, "test");
        let s = String::from_utf8(buf).unwrap();
        let marker_count = s.matches("<<<<<<< local").count();
        assert_eq!(marker_count, 3, "expected 3 blocks, got {marker_count}: {s}");
    }

    #[test]
    fn build_conflict_buffer_preserves_trailing_newline_consistency() {
        // Always-trailing-newline (or empty) invariant: cover a few
        // shapes (newline-terminated, no-trailing-newline, single line,
        // and the all-equal short-circuit).
        for (l, r) in [
            (&b"a\nb\n"[..], &b"a\nc\n"[..]),
            (&b"a"[..], &b"b"[..]),
            (&b""[..], &b"x\n"[..]),
            (&b"same\n"[..], &b"same\n"[..]),
        ] {
            let buf = build_conflict_buffer(l, r, "test");
            let s = String::from_utf8(buf).unwrap();
            assert!(
                s.is_empty() || s.ends_with('\n'),
                "expected newline-terminated or empty: {s:?}"
            );
        }
    }

    // ============================================================
    // Group B — prompt_hunk_by_hunk adversarial examples.
    // ============================================================

    #[test]
    fn hunk_by_hunk_eof_mid_walk_preserves_partial_work_with_markers() {
        // 3 hunks; only one decision typed. Walker should resolve hunk
        // 1 from stdin, then hit EOF mid-walk and mark hunks 2 and 3
        // as Skip — yielding EditedWithMarkers with both hunks wrapped
        // in markers.
        let (local, remote) = three_hunk_fixture();
        let mut output: Vec<u8> = Vec::new();
        let mut input = Cursor::new(b"k\n".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "production", ColorMode::Plain,
        ).unwrap();
        let bytes = match outcome {
            EditOutcome::EditedWithMarkers(b) => b,
            other => panic!("expected EditedWithMarkers, got {other:?}"),
        };
        let s = String::from_utf8_lossy(&bytes);
        // Hunk 1 kept local (FOO present, foo absent).
        assert!(s.contains("\nFOO\n"), "hunk 1 missing local: {s}");
        // Hunk 2 and 3 wrapped in markers (BAR/bar and BAZ/baz).
        let count = s.matches("<<<<<<< local").count();
        assert_eq!(count, 2, "expected 2 wrapped hunks, got {count}: {s}");
        assert!(s.contains("BAR\n=======\nbar\n"), "hunk 2 wrap: {s}");
        assert!(s.contains("BAZ\n=======\nbaz\n"), "hunk 3 wrap: {s}");
    }

    #[test]
    fn hunk_by_hunk_unknown_action_reprompts() {
        // 1 hunk; user types `x\n` (unknown), then `k\n`.
        let local = b"a\nLOCAL\nb\n".to_vec();
        let remote = b"a\nREMOTE\nb\n".to_vec();
        let mut output: Vec<u8> = Vec::new();
        let mut input = Cursor::new(b"x\nk\n".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "test", ColorMode::Plain,
        ).unwrap();
        match outcome {
            EditOutcome::Edited(bytes) => assert_eq!(bytes, local),
            other => panic!("expected Edited, got {other:?}"),
        }
        let s = String::from_utf8(output).unwrap();
        assert!(s.contains("unrecognized"), "unknown should re-prompt: {s}");
    }

    #[test]
    fn hunk_by_hunk_immediate_abort_returns_aborted() {
        let (local, remote) = three_hunk_fixture();
        let mut output: Vec<u8> = Vec::new();
        let mut input = Cursor::new(b"a\n".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "test", ColorMode::Plain,
        ).unwrap();
        assert!(matches!(outcome, EditOutcome::Aborted), "got {outcome:?}");
    }

    #[test]
    fn hunk_by_hunk_mixed_options_handle_in_order() {
        // 4-hunk fixture. Decisions: k, r, b, s.
        let local = b"a\nFOO\nb\nBAR\nc\nBAZ\nd\nQUX\ne\n".to_vec();
        let remote = b"a\nfoo\nb\nbar\nc\nbaz\nd\nqux\ne\n".to_vec();
        let mut output: Vec<u8> = Vec::new();
        let mut input = Cursor::new(b"k\nr\nb\ns\n".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "production", ColorMode::Plain,
        ).unwrap();
        let bytes = match outcome {
            EditOutcome::EditedWithMarkers(b) => b,
            other => panic!("expected EditedWithMarkers (one [s] used), got {other:?}"),
        };
        // Expected: equal lines (a, b, c, d, e) preserved; hunk 1 = local
        // (FOO); hunk 2 = remote (bar); hunk 3 = both (BAZ then baz);
        // hunk 4 = markers around QUX/qux.
        let expected =
            b"a\nFOO\nb\nbar\nc\nBAZ\nbaz\nd\n<<<<<<< local\nQUX\n=======\nqux\n>>>>>>> production\ne\n"
                .to_vec();
        assert_eq!(
            bytes,
            expected,
            "got {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    #[test]
    fn hunk_by_hunk_identical_inputs_short_circuits_to_local() {
        // No conflict hunks at all: walker must return Edited(local) without
        // reading from stdin or prompting.
        let local = b"a\nb\nc\n".to_vec();
        let remote = local.clone();
        let mut output: Vec<u8> = Vec::new();
        let mut input = Cursor::new(b"".to_vec());
        let path = std::path::Path::new("x.py");
        let outcome = prompt_hunk_by_hunk(
            &mut input, &mut output, &local, &remote, path, "test", ColorMode::Plain,
        ).unwrap();
        match outcome {
            EditOutcome::Edited(bytes) => assert_eq!(bytes, local),
            other => panic!("expected Edited(local), got {other:?}"),
        }
        // No prompt should have been rendered.
        let s = String::from_utf8(output).unwrap();
        assert!(s.is_empty(), "no prompt expected: {s:?}");
    }

    // ============================================================
    // Group C — validator coverage.
    // ============================================================

    #[test]
    fn validate_edited_catches_indented_marker() {
        // An indented marker would otherwise sneak into the lockfile.
        // We catch it via trim_start().
        let s = b"def x():\n    <<<<<<< sneaky\n    pass\n";
        let err = validate_edited(s, std::path::Path::new("x.py")).unwrap_err();
        assert!(err.contains("conflict marker"), "got: {err}");
    }

    #[test]
    fn validate_edited_catches_marker_at_end_of_file() {
        // Last line is a marker without trailing newline.
        let s = b"foo\n>>>>>>> remote";
        let err = validate_edited(s, std::path::Path::new("x.py")).unwrap_err();
        assert!(err.contains("conflict marker"), "got: {err}");
    }

    #[test]
    fn validate_edited_json_path_rejects_invalid_json() {
        let err = validate_edited(b"{not json", std::path::Path::new("x.json")).unwrap_err();
        assert!(err.contains("not valid JSON"), "got: {err}");
    }

    #[test]
    fn validate_edited_json_path_accepts_valid_json() {
        validate_edited(b"{\"a\":1}", std::path::Path::new("x.json")).unwrap();
    }

    #[test]
    fn validate_edited_py_path_skips_json_validation() {
        // Same bytes that fail-as-JSON pass as `.py` content.
        validate_edited(b"def x(): pass\n", std::path::Path::new("x.py")).unwrap();
    }

    #[test]
    fn validate_edited_rejects_non_utf8() {
        let s: &[u8] = &[b'a', 0xFF, b'b'];
        let err = validate_edited(s, std::path::Path::new("x.py")).unwrap_err();
        assert!(err.contains("UTF-8"), "got: {err}");
    }

    #[test]
    fn validate_markers_only_catches_indented_marker() {
        // Same indented-marker fix applies to the per-hunk validator.
        let s = b"def x():\n    ======= sneaky\n";
        let err = validate_edited_markers_only(s).unwrap_err();
        assert!(err.contains("conflict marker"), "got: {err}");
    }

    // ============================================================
    // Group D — resolver UI / prompt_resolve_with_color.
    // ============================================================

    #[test]
    fn prompt_resolve_header_includes_hunk_count_for_multi_hunk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.py");
        std::fs::write(&path, b"a\nFOO\nb\nBAR\nc\nBAZ\nd\n").unwrap();
        let mut output: Vec<u8> = Vec::new();
        let input = Cursor::new(b"s\n");
        prompt_resolve_with_color(
            input,
            &mut output,
            1,
            1,
            &path,
            b"a\nfoo\nb\nbar\nc\nbaz\nd\n",
            "test",
            ColorMode::Plain,
        )
        .unwrap();
        let s = String::from_utf8(output).unwrap();
        assert!(s.contains("(3 hunks)"), "header should advertise hunk count: {s}");
    }

    #[test]
    fn prompt_resolve_header_omits_hunk_count_for_single_hunk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.py");
        std::fs::write(&path, b"a\nLOCAL\nb\n").unwrap();
        let mut output: Vec<u8> = Vec::new();
        let input = Cursor::new(b"s\n");
        prompt_resolve_with_color(
            input,
            &mut output,
            1,
            1,
            &path,
            b"a\nREMOTE\nb\n",
            "test",
            ColorMode::Plain,
        )
        .unwrap();
        let s = String::from_utf8(output).unwrap();
        assert!(!s.contains("hunks)"), "single-hunk header must omit count: {s}");
    }

    #[test]
    fn prompt_resolve_unknown_key_reprompts_includes_h_for_multi_hunk() {
        // 3-hunk conflict; stdin `z\n` (unrecognized) then `s\n`.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.py");
        std::fs::write(&path, b"a\nFOO\nb\nBAR\nc\nBAZ\nd\n").unwrap();
        let mut output: Vec<u8> = Vec::new();
        let input = Cursor::new(b"z\ns\n");
        let r = prompt_resolve_with_color(
            input,
            &mut output,
            1,
            1,
            &path,
            b"a\nfoo\nb\nbar\nc\nbaz\nd\n",
            "test",
            ColorMode::Plain,
        )
        .unwrap();
        assert!(matches!(r, Resolution::Skip));
        let s = String::from_utf8(output).unwrap();
        assert!(s.contains("unrecognized"), "should print unrecognized: {s}");
        let prompts = s.matches("[k] keep local").count();
        assert!(prompts >= 2, "should have re-prompted at least twice: count={prompts}, output={s}");
        assert!(s.contains("[h] hunk-by-hunk"), "multi-hunk re-prompt should include [h]: {s}");
    }
}
