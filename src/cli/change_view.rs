//! The one renderer behind every change rdc shows the user.
//!
//! Spec: docs/superpowers/specs/2026-09-01-unified-change-rendering-design.md
//!
//! Three parts, and every in-scope surface is built from these and nothing
//! else:
//!
//! ```text
//! <event line>     HH:MM:SS <action> <prose>     Log::event
//!   <row>          one per object                render_row
//!     <expansion>  the diff body                 render_diff_body
//! ```
//!
//! The row's verb sits in the same column as the event line's action token, so
//! a plan reads as pre-filled log lines rather than a bullet list bolted
//! underneath one.
//!
//! `ColorMode::Plain` renders IDENTICAL glyphs and columns to `Color` — the
//! only difference is the SGR. That is what makes a CI log match what the same
//! command printed locally.

use crate::cli::resolve::{
    ColorMode, SGR_ADD_BOLD, SGR_DIM, SGR_REMOVE_BOLD, SGR_RESET, colorize_error,
    colorize_final_ok, colorize_success, colorize_warning, line_diff,
};

// --- layout constants -------------------------------------------------------

/// Leading indent for a row and for a diff body. Nine characters is exactly
/// `"HH:MM:SS "`, so a row's verb lands in the event line's action column.
const INDENT: usize = 9;
/// Width of the verb token, matching `log::Action::pad`.
const VERB_W: usize = 6;
/// Width of each of the two count columns (`+N` and `-N`).
const COUNT_W: usize = 6;
/// Gap between the count block and the note.
const NOTE_GAP: usize = 3;

// --- diff palette -----------------------------------------------------------
//
// Row backgrounds use truecolor + the EL trick (`\x1b[K`): once a background is
// active, erase-to-end-of-line fills the rest of the row with it, so a
// removed/added row tints edge-to-edge regardless of content width. Foreground
// tokens inside a tinted row end with `\x1b[39m` (reset fg, keep bg) — never a
// full `\x1b[0m` — so the bg survives until the trailing EL + reset.
pub(crate) const SGR_BG_ADD: &str = "\x1b[48;2;20;48;28m"; // deep green (added row)
pub(crate) const SGR_BG_REMOVE: &str = "\x1b[48;2;60;24;26m"; // deep red (removed row)
pub(crate) const SGR_BG_ADD_HI: &str = "\x1b[48;2;38;92;52m"; // brighter green — changed span
pub(crate) const SGR_BG_REMOVE_HI: &str = "\x1b[48;2;120;42;46m"; // brighter red — changed span
pub(crate) const SGR_GUTTER: &str = "\x1b[38;2;120;120;120m"; // gray line numbers (context)
pub(crate) const SGR_GUTTER_ADD: &str = "\x1b[38;2;135;190;120m"; // green line number (added)
pub(crate) const SGR_GUTTER_REMOVE: &str = "\x1b[38;2;225;130;130m"; // red line number (removed)
pub(crate) const SGR_FG_DEFAULT: &str = "\x1b[39m"; // reset fg, preserve bg
pub(crate) const SGR_EOL: &str = "\x1b[K"; // erase to EOL → fills current bg
pub(crate) const SGR_J_KEY: &str = "\x1b[38;2;126;167;255m"; // JSON keys
pub(crate) const SGR_J_STR: &str = "\x1b[38;2;152;195;121m"; // JSON string values
pub(crate) const SGR_J_NUM: &str = "\x1b[38;2;229;181;103m"; // JSON numbers
pub(crate) const SGR_J_KW: &str = "\x1b[38;2;198;146;233m"; // true / false / null

// --- rows -------------------------------------------------------------------

/// What a row says rdc will do (or did) to one object.
///
/// The colour of each verb reuses `log::ActionColor`'s existing buckets, so a
/// row's verb reads the same way the executor's action token does: writes are
/// bold green, destructive red, reads plain green, anything needing the user
/// amber.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RowVerb {
    /// `LocalEdit` — PATCH the remote.
    Patch,
    /// `LocalCreate` — POST to the remote.
    Post,
    /// `LocalDelete` or a tombstone — DELETE on the remote.
    Delete,
    /// `RemoteEdit` / `RemoteCreate` / `RemoteDelete` — write locally.
    Pull,
    /// Any class that stops to ask: `BothDiverged`,
    /// `LocalEditRemoteDelete`, `LocalDeleteRemoteEdit`.
    Prompt,
    /// An MDH index that would be dropped.
    Drop,
}

impl RowVerb {
    /// The bare verb. [`render_row`] pads it to [`VERB_W`], so the width lives
    /// in exactly one place and a future verb cannot shift the kind column.
    pub fn token(self) -> &'static str {
        match self {
            RowVerb::Patch => "patch",
            RowVerb::Post => "post",
            RowVerb::Delete => "delete",
            RowVerb::Pull => "pull",
            RowVerb::Prompt => "prompt",
            RowVerb::Drop => "drop",
        }
    }

    fn colorize(self, mode: ColorMode) -> String {
        let t = self.token();
        match self {
            RowVerb::Patch | RowVerb::Post => colorize_final_ok(t, mode),
            RowVerb::Delete | RowVerb::Drop => colorize_error(t, mode),
            RowVerb::Pull => colorize_success(t, mode),
            RowVerb::Prompt => colorize_warning(t, mode),
        }
    }
}

/// One object's line in a plan, a gate, or above a diff.
pub struct ChangeRow<'a> {
    pub verb: RowVerb,
    /// Plural registry name, verbatim from `kinds.rs` — never hand-built, which
    /// is what stops the plan and the executor disagreeing about whether it is
    /// `hooks` or `hook`.
    pub kind: &'a str,
    /// The object's slug. Compound slugs (`email_templates` is
    /// `<ws>/<queue>/<name>`, `engine_fields` is `<engine>/<field>`) render
    /// their container segments dim and the leaf at full weight.
    pub name: &'a str,
    /// Lines added / removed. `None` on both where a line count is meaningless
    /// (an MDH index has no body), which renders the columns blank.
    pub added: Option<usize>,
    pub removed: Option<usize>,
    pub note: Option<&'a str>,
}

/// Column widths shared by every row in one sync cycle.
///
/// Sized to the widest entry rather than fixed, which is possible because
/// `classified` is fully built before `execute::run` receives it — so the plan,
/// the interactive prompts and the executed rows all line up.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RowWidths {
    pub kind: usize,
    pub name: usize,
}

impl RowWidths {
    pub const MIN_KIND: usize = 8;
    pub const MIN_NAME: usize = 12;
    /// Past this a name middle-elides, keeping the leaf.
    pub const MAX_NAME: usize = 40;

    /// Fit to `(kind, name)` pairs, clamped to the minimums and the name cap.
    pub fn fit<'a, I: IntoIterator<Item = (&'a str, &'a str)>>(pairs: I) -> Self {
        let (mut k, mut n) = (Self::MIN_KIND, Self::MIN_NAME);
        for (kind, name) in pairs {
            k = k.max(kind.chars().count());
            n = n.max(name.chars().count());
        }
        RowWidths {
            kind: k,
            name: n.min(Self::MAX_NAME),
        }
    }
}

/// Shorten `name` to at most `max` characters, keeping the leaf segment — the
/// part that identifies the object — and eliding the containers before it.
pub fn elide_name(name: &str, max: usize) -> String {
    let n = name.chars().count();
    if n <= max {
        return name.to_string();
    }
    let leaf: String = match name.rfind('/') {
        Some(i) => name[i + 1..].to_string(),
        None => name.to_string(),
    };
    let leaf_len = leaf.chars().count();
    // `…/` costs two; if the leaf alone cannot fit beside it, drop the
    // containers entirely and trim the leaf from the left.
    if leaf_len + 2 >= max {
        let keep = max.saturating_sub(1);
        let tail: String = leaf.chars().skip(leaf_len.saturating_sub(keep)).collect();
        return format!("\u{2026}{tail}");
    }
    let head: String = name.chars().take(max - leaf_len - 2).collect();
    format!("{head}\u{2026}/{leaf}")
}

/// Render the name with its container segments dimmed and the leaf at full
/// weight — the same "dim the container, keep the identity" rule the kind
/// column follows, applied one level down.
fn render_name(name: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return name.to_string();
    }
    match name.rfind('/') {
        Some(i) => format!("{SGR_DIM}{}{SGR_RESET}{}", &name[..=i], &name[i + 1..]),
        None => name.to_string(),
    }
}

/// Render one count column: `+N`, `-N`, or a dim `0`.
fn render_count(n: usize, sign: char, mode: ColorMode) -> String {
    let token = if n == 0 {
        "0".to_string()
    } else {
        format!("{sign}{n}")
    };
    let pad = " ".repeat(COUNT_W.saturating_sub(token.chars().count()));
    if mode == ColorMode::Plain {
        return format!("{pad}{token}");
    }
    let painted = if n == 0 {
        format!("{SGR_DIM}{token}{SGR_RESET}")
    } else if sign == '+' {
        format!("{SGR_ADD_BOLD}{token}{SGR_RESET}")
    } else {
        format!("{SGR_REMOVE_BOLD}{token}{SGR_RESET}")
    };
    format!("{pad}{painted}")
}

/// Render one change row. See the module docs for the column contract.
pub fn render_row(row: &ChangeRow<'_>, w: RowWidths, mode: ColorMode) -> String {
    let plain = mode == ColorMode::Plain;
    let mut out = " ".repeat(INDENT);

    out.push_str(&row.verb.colorize(mode));
    out.push_str(&" ".repeat(VERB_W.saturating_sub(row.verb.token().chars().count())));
    out.push(' ');

    if plain {
        out.push_str(row.kind);
    } else {
        out.push_str(&format!("{SGR_DIM}{}{SGR_RESET}", row.kind));
    }
    out.push_str(&" ".repeat(w.kind.saturating_sub(row.kind.chars().count())));
    out.push(' ');

    let name = elide_name(row.name, w.name);
    out.push_str(&render_name(&name, mode));
    out.push_str(&" ".repeat(w.name.saturating_sub(name.chars().count())));

    match (row.added, row.removed) {
        (Some(a), Some(r)) => {
            out.push_str(&render_count(a, '+', mode));
            out.push_str(&render_count(r, '-', mode));
        }
        _ => out.push_str(&" ".repeat(COUNT_W * 2)),
    }

    if let Some(note) = row.note {
        out.push_str(&" ".repeat(NOTE_GAP));
        if plain {
            out.push_str(note);
        } else {
            out.push_str(&format!("{SGR_DIM}{note}{SGR_RESET}"));
        }
    }

    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// The line between a row and its diff body: the file path (directory dimmed,
/// filename at full weight) and, when both annotations are non-empty, the
/// `- left  + right` side legend.
///
/// This replaces the old `Update(<path>)` header + `Added N lines, removed M
/// lines` summary, both of which now only repeat what the row above already
/// says.
pub fn render_connector(
    path: &std::path::Path,
    left: &str,
    right: &str,
    mode: ColorMode,
) -> String {
    let plain = mode == ColorMode::Plain;
    let shown = path.display().to_string();
    let painted_path = if plain {
        shown.clone()
    } else {
        match shown.rfind('/') {
            Some(i) => format!("{SGR_DIM}{}{SGR_RESET}{}", &shown[..=i], &shown[i + 1..]),
            None => shown.clone(),
        }
    };
    let mut out = " ".repeat(INDENT);
    if plain {
        out.push_str("\u{23bf} ");
    } else {
        out.push_str(&format!("{SGR_DIM}\u{23bf}{SGR_RESET} "));
    }
    out.push_str(&painted_path);
    if !left.is_empty() && !right.is_empty() {
        if plain {
            out.push_str(&format!("   - {left}  + {right}"));
        } else {
            out.push_str(&format!(
                "   {SGR_REMOVE_BOLD}- {left}{SGR_RESET}  {SGR_ADD_BOLD}+ {right}{SGR_RESET}"
            ));
        }
    }
    out
}

/// Count added / removed lines between two texts — the numbers a row's `±`
/// columns show. This is what the diff body's old `Added N lines, removed M
/// lines` summary used to say; it now lives on the row instead.
pub fn count_changes(left: &str, right: &str) -> (usize, usize) {
    use similar::ChangeTag;
    let diff = line_diff(left, right);
    let (mut added, mut removed) = (0usize, 0usize);
    for op in diff.grouped_ops(3).iter().flatten() {
        for ch in diff.iter_changes(op) {
            match ch.tag() {
                ChangeTag::Insert => added += 1,
                ChangeTag::Delete => removed += 1,
                ChangeTag::Equal => {}
            }
        }
    }
    (added, removed)
}

// --- diff body --------------------------------------------------------------

/// JSON syntax-highlight spans for one line: `(start, end, fg)` byte ranges for
/// `"keys"` (a string immediately followed by `:`), `"string values"`, numbers,
/// and the literals `true`/`false`/`null`. Bytes not covered by any span render
/// in the default foreground. Best-effort and line-local; never panics.
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
            let color = if j < b.len() && b[j] == b':' {
                SGR_J_KEY
            } else {
                SGR_J_STR
            };
            spans.push((start, i, color));
        } else if c.is_ascii_digit() || (c == b'-' && i + 1 < b.len() && b[i + 1].is_ascii_digit())
        {
            let start = i;
            i += 1;
            while i < b.len()
                && (b[i].is_ascii_digit() || matches!(b[i], b'.' | b'e' | b'E' | b'+' | b'-'))
            {
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
            let ch = line[i..].chars().next().expect("index is a char boundary");
            i += ch.len_utf8();
        }
    }
    spans
}

/// Render one diff row's content (everything after the gutter + marker),
/// combining two overlays: JSON syntax highlighting (foreground) and intra-line
/// change emphasis — a brighter background (`hi_bg`) over the `emph` byte
/// ranges, which are the substrings that actually differ from the paired row.
/// `base_bg` is `Some` for changed (`-`/`+`) rows and `None` for context rows.
/// Foreground changes use `\x1b[39m` so the active background is never
/// disturbed; the background is restored to `base_bg` before returning.
fn render_content(
    content: &str,
    is_json: bool,
    base_bg: Option<&str>,
    hi_bg: &str,
    emph: &[(usize, usize)],
) -> String {
    let fg = if is_json {
        json_fg_spans(content)
    } else {
        Vec::new()
    };
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
        let seg_fg = fg
            .iter()
            .find(|(s, e, _)| *s <= a && a < *e)
            .map(|(_, _, c)| *c);
        let seg_emph = emph.iter().any(|(s, e)| *s <= a && a < *e);
        if let Some(bg) = base_bg
            && seg_emph != cur_emph
        {
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
        && cur_emph
    {
        out.push_str(bg);
    }
    out
}

/// One diff row, already reassembled: which line numbers it carries, whether it
/// is a delete/insert/context, its text, and the byte ranges that differ from
/// the paired row.
struct DiffRow {
    old: Option<usize>,
    new: Option<usize>,
    tag: similar::ChangeTag,
    content: String,
    emph: Vec<(usize, usize)>,
}

/// Render the body of a diff: line-numbered hunks with three lines of context,
/// row backgrounds, intra-line emphasis and (for JSON) syntax highlighting.
///
/// No header and no `Added N lines` summary — the row above the body already
/// carries the verb, the object and the counts.
///
/// Two properties the old renderer did not have:
///
/// - **A split gutter.** Old and new line numbers get their own column, so a
///   number never appears twice in a row meaning two different things.
/// - **Interleaved pairs.** `similar` emits a Replace as every delete followed
///   by every insert; the old and new spelling of one field are zipped back
///   together here so a change reads as one unit.
///
/// Returns `""` when the two sides are byte-identical.
pub fn render_diff_body(left: &str, right: &str, is_json: bool, mode: ColorMode) -> String {
    use similar::ChangeTag;
    use std::fmt::Write as _;

    let diff = line_diff(left, right);
    let groups = diff.grouped_ops(3);
    if groups.is_empty() {
        return String::new();
    }

    let mut max_line = 1usize;
    for op in groups.iter().flatten() {
        for ch in diff.iter_changes(op) {
            if let Some(i) = ch.old_index() {
                max_line = max_line.max(i + 1);
            }
            if let Some(i) = ch.new_index() {
                max_line = max_line.max(i + 1);
            }
        }
    }
    let w = max_line.to_string().len().max(3);
    let plain = mode == ColorMode::Plain;
    let ind = " ".repeat(INDENT);

    let mut out = String::new();
    for (gi, group) in groups.iter().enumerate() {
        if gi > 0 {
            let _ = if plain {
                writeln!(out, "{ind}\u{22ee}")
            } else {
                writeln!(out, "{ind}{SGR_DIM}\u{22ee}{SGR_RESET}")
            };
        }
        for op in group {
            // Reassemble every change in this op first, then reorder. `similar`
            // emits a Replace as every Delete followed by every Insert; zipping
            // them back together puts a changed field's old and new spelling
            // next to each other, which is where its intra-line emphasis is
            // actually readable.
            let mut rows: Vec<DiffRow> = Vec::new();
            for change in diff.iter_inline_changes(op) {
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
                rows.push(DiffRow {
                    old: change.old_index().map(|i| i + 1),
                    new: change.new_index().map(|i| i + 1),
                    tag: change.tag(),
                    content,
                    emph,
                });
            }

            let has_del = rows.iter().any(|r| r.tag == ChangeTag::Delete);
            let has_ins = rows.iter().any(|r| r.tag == ChangeTag::Insert);
            if has_del && has_ins {
                let (dels, inss): (Vec<DiffRow>, Vec<DiffRow>) =
                    rows.into_iter().partition(|r| r.tag == ChangeTag::Delete);
                let (mut d, mut a) = (dels.into_iter(), inss.into_iter());
                let mut zipped = Vec::new();
                loop {
                    let (nd, na) = (d.next(), a.next());
                    if nd.is_none() && na.is_none() {
                        break;
                    }
                    zipped.extend(nd);
                    zipped.extend(na);
                }
                rows = zipped;
            }

            for r in &rows {
                let num = |n: Option<usize>| match n {
                    Some(v) => format!("{v:>w$}"),
                    None => " ".repeat(w),
                };
                let gutter = format!("{} {}", num(r.old), num(r.new));
                let marker = match r.tag {
                    ChangeTag::Equal => ' ',
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                };
                if plain {
                    let _ = writeln!(out, "{ind}{gutter} \u{2502} {marker} {}", r.content);
                    continue;
                }
                let _ = match r.tag {
                    ChangeTag::Delete => {
                        let body = render_content(
                            &r.content,
                            is_json,
                            Some(SGR_BG_REMOVE),
                            SGR_BG_REMOVE_HI,
                            &r.emph,
                        );
                        writeln!(
                            out,
                            "{SGR_BG_REMOVE}{ind}{SGR_GUTTER_REMOVE}{gutter} \u{2502} {marker}\
                             {SGR_FG_DEFAULT} {body}{SGR_EOL}{SGR_RESET}"
                        )
                    }
                    ChangeTag::Insert => {
                        let body = render_content(
                            &r.content,
                            is_json,
                            Some(SGR_BG_ADD),
                            SGR_BG_ADD_HI,
                            &r.emph,
                        );
                        writeln!(
                            out,
                            "{SGR_BG_ADD}{ind}{SGR_GUTTER_ADD}{gutter} \u{2502} {marker}\
                             {SGR_FG_DEFAULT} {body}{SGR_EOL}{SGR_RESET}"
                        )
                    }
                    ChangeTag::Equal => {
                        let body = render_content(&r.content, is_json, None, "", &r.emph);
                        writeln!(out, "{ind}{SGR_GUTTER}{gutter} \u{2502}{SGR_RESET}   {body}")
                    }
                };
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Strip every SGR sequence (`\x1b[…m`) and the erase-to-EOL (`\x1b[K`).
    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            if it.peek() == Some(&'[') {
                it.next();
                for t in it.by_ref() {
                    if t.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        }
        out
    }

    fn w() -> RowWidths {
        RowWidths {
            kind: 15,
            name: 22,
        }
    }

    fn sample<'a>(note: Option<&'a str>) -> ChangeRow<'a> {
        ChangeRow {
            verb: RowVerb::Patch,
            kind: "rules",
            name: "finance-totals",
            added: Some(4),
            removed: Some(4),
            note,
        }
    }

    #[test]
    fn row_columns_land_at_fixed_offsets() {
        let r = render_row(&sample(Some("412ms")), w(), ColorMode::Plain);
        assert_eq!(&r[0..9], " ".repeat(9), "indent");
        assert_eq!(&r[9..15], "patch ", "verb column");
        assert_eq!(&r[16..31], "rules          ", "kind column");
        assert_eq!(&r[32..54], "finance-totals        ", "name column");
        assert_eq!(&r[54..60], "    +4", "added column");
        assert_eq!(&r[60..66], "    -4", "removed column");
        assert_eq!(&r[69..], "412ms", "note");
    }

    #[test]
    fn verb_sits_in_the_event_lines_action_column() {
        // `log::Action::pad` is 6 wide and starts at byte 9 of an event line
        // ("HH:MM:SS "). A row's verb has to occupy exactly that span or the
        // plan stops reading as pre-filled log lines.
        for v in [
            RowVerb::Patch,
            RowVerb::Post,
            RowVerb::Delete,
            RowVerb::Pull,
            RowVerb::Prompt,
            RowVerb::Drop,
        ] {
            assert!(
                v.token().len() <= VERB_W,
                "{v:?} token {:?} overflows the {VERB_W}-char action column",
                v.token()
            );
        }
    }

    #[test]
    fn kind_is_dim_and_name_is_not() {
        let r = render_row(&sample(None), w(), ColorMode::Color);
        assert!(
            r.contains(&format!("{SGR_DIM}rules{SGR_RESET}")),
            "kind should be dim: {r:?}"
        );
        assert!(
            !r.contains(&format!("{SGR_DIM}finance-totals")),
            "name must not be dim: {r:?}"
        );
    }

    #[test]
    fn compound_name_dims_container_segments() {
        let row = ChangeRow {
            verb: RowVerb::Patch,
            kind: "email_templates",
            name: "main/invoices/reminder",
            added: Some(2),
            removed: Some(2),
            note: None,
        };
        let r = render_row(&row, w(), ColorMode::Color);
        assert!(
            r.contains(&format!("{SGR_DIM}main/invoices/{SGR_RESET}reminder")),
            "containers dim, leaf full weight: {r:?}"
        );
    }

    #[test]
    fn absent_counts_render_blank() {
        let row = ChangeRow {
            verb: RowVerb::Drop,
            kind: "mdh",
            name: "vendors",
            added: None,
            removed: None,
            note: Some("regular index idx_vendor_no"),
        };
        let r = render_row(&row, w(), ColorMode::Plain);
        assert_eq!(&r[54..66], " ".repeat(12), "count columns blank: {r:?}");
        assert_eq!(&r[69..], "regular index idx_vendor_no");
    }

    #[test]
    fn zero_counts_render_a_bare_zero() {
        let row = ChangeRow {
            verb: RowVerb::Delete,
            kind: "hooks",
            name: "legacy-export",
            added: Some(0),
            removed: Some(38),
            note: None,
        };
        let r = render_row(&row, w(), ColorMode::Plain);
        assert_eq!(&r[54..60], "     0");
        assert_eq!(&r[60..66], "   -38");
    }

    #[test]
    fn plain_is_color_minus_sgr() {
        for row in [
            sample(Some("412ms")),
            sample(None),
            ChangeRow {
                verb: RowVerb::Drop,
                kind: "mdh",
                name: "a/b/c",
                added: None,
                removed: None,
                note: Some("n"),
            },
        ] {
            let plain = render_row(&row, w(), ColorMode::Plain);
            let color = render_row(&row, w(), ColorMode::Color);
            assert_eq!(strip_sgr(&color), plain, "plain must be color minus SGR");
            assert!(!plain.contains('\u{1b}'), "plain leaked SGR: {plain:?}");
        }
    }

    #[test]
    fn widths_fit_to_the_batch_with_minimums() {
        let tiny = RowWidths::fit([("mdh", "a")]);
        assert_eq!(
            tiny,
            RowWidths {
                kind: RowWidths::MIN_KIND,
                name: RowWidths::MIN_NAME
            }
        );
        let real = RowWidths::fit([
            ("rules", "finance-totals"),
            ("email_templates", "main/invoices/reminder"),
        ]);
        assert_eq!(real, RowWidths { kind: 15, name: 22 });
    }

    #[test]
    fn widths_cap_the_name_column() {
        let long = "a/".repeat(40) + "leaf";
        let fitted = RowWidths::fit([("hooks", long.as_str())]);
        assert_eq!(fitted.name, RowWidths::MAX_NAME);
    }

    #[test]
    fn long_name_middle_elides_preserving_leaf() {
        let got = elide_name("main/invoices/rejection-default", 20);
        assert_eq!(got, "m\u{2026}/rejection-default");
        assert_eq!(got.chars().count(), 20);
        // A leaf that cannot fit beside the containers drops them entirely.
        let got = elide_name("ws/a-very-long-single-leaf-name", 10);
        assert_eq!(got.chars().count(), 10);
        assert!(got.starts_with('\u{2026}'), "{got:?}");
        // Short enough is untouched.
        assert_eq!(elide_name("orders", 22), "orders");
    }

    // --- diff body ---------------------------------------------------------

    const L: &str = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
    const R: &str = "{\n  \"a\": 9,\n  \"b\": 8\n}\n";

    /// Markers in emission order: the char two positions after the `│` rule.
    fn markers(out: &str) -> Vec<char> {
        out.lines()
            .filter_map(|l| {
                let i = l.find('\u{2502}')?;
                l[i..].chars().nth(2)
            })
            .collect()
    }

    #[test]
    fn identical_sides_render_empty() {
        assert!(render_diff_body(L, L, true, ColorMode::Plain).is_empty());
    }

    #[test]
    fn body_carries_no_header_or_summary() {
        let out = render_diff_body(L, R, true, ColorMode::Plain);
        assert!(!out.contains("Update("), "header must be gone: {out}");
        assert!(!out.contains("Added "), "summary must be gone: {out}");
        assert!(!out.contains("\u{23bf}"), "connector belongs to the caller: {out}");
    }

    #[test]
    fn replace_pairs_interleave() {
        let out = render_diff_body(L, R, true, ColorMode::Plain);
        assert_eq!(
            markers(&out),
            vec![' ', '-', '+', '-', '+', ' '],
            "old/new pairs must be adjacent:\n{out}"
        );
    }

    #[test]
    fn gutter_splits_old_and_new_columns() {
        let out = render_diff_body(L, R, true, ColorMode::Plain);
        let lines: Vec<&str> = out.lines().collect();
        // Context row carries both numbers.
        assert_eq!(&lines[0][9..16], "  1   1", "context gutter: {:?}", lines[0]);
        // Delete carries old only; insert carries new only.
        assert_eq!(&lines[1][9..16], "  2    ", "delete gutter: {:?}", lines[1]);
        assert_eq!(&lines[2][9..16], "      2", "insert gutter: {:?}", lines[2]);
        // The same number therefore never appears twice meaning two things.
        assert_ne!(&lines[1][9..16], &lines[2][9..16]);
    }

    #[test]
    fn json_highlighting_only_for_json() {
        let json = render_diff_body(L, R, true, ColorMode::Color);
        assert!(json.contains(SGR_J_KEY), "json keys should be highlighted");
        let py = render_diff_body("a = 1\n", "a = 2\n", false, ColorMode::Color);
        assert!(
            !py.contains(SGR_J_KEY) && !py.contains(SGR_J_NUM),
            "non-json must skip syntax highlighting: {py:?}"
        );
    }

    #[test]
    fn changed_rows_tint_edge_to_edge_with_intra_line_emphasis() {
        let out = render_diff_body(L, R, true, ColorMode::Color);
        assert!(out.contains(SGR_BG_REMOVE) && out.contains(SGR_BG_ADD));
        assert!(out.contains(SGR_EOL), "rows must fill the bg to the line end");
        assert!(out.contains(SGR_BG_REMOVE_HI) && out.contains(SGR_BG_ADD_HI));
        assert!(out.contains(SGR_GUTTER_REMOVE) && out.contains(SGR_GUTTER_ADD));
    }

    #[test]
    fn plain_body_is_color_minus_sgr() {
        let plain = render_diff_body(L, R, true, ColorMode::Plain);
        let color = render_diff_body(L, R, true, ColorMode::Color);
        assert!(!plain.contains('\u{1b}'), "plain leaked SGR: {plain:?}");
        assert_eq!(strip_sgr(&color), plain, "plain must be color minus SGR");
    }

    #[test]
    fn one_sided_input_renders_every_line_as_a_deletion() {
        let out = render_diff_body(L, "", true, ColorMode::Plain);
        assert!(markers(&out).iter().all(|m| *m == '-'), "{out}");
    }

    #[test]
    fn count_changes_matches_the_body() {
        assert_eq!(count_changes(L, R), (2, 2));
        assert_eq!(count_changes(L, L), (0, 0));
        assert_eq!(count_changes(L, ""), (0, 4));
        assert_eq!(count_changes("", R), (4, 0));
    }

    // --- connector ---------------------------------------------------------

    #[test]
    fn connector_dims_the_directory() {
        let p = std::path::Path::new("workspaces/main/queues/invoices/queue.json");
        let c = render_connector(p, "local", "test", ColorMode::Color);
        assert!(
            c.contains(&format!(
                "{SGR_DIM}workspaces/main/queues/invoices/{SGR_RESET}queue.json"
            )),
            "directory dim, filename full weight: {c:?}"
        );
        assert!(c.contains(&format!("{SGR_REMOVE_BOLD}- local")));
        assert!(c.contains(&format!("{SGR_ADD_BOLD}+ test")));
        let plain = render_connector(p, "local", "test", ColorMode::Plain);
        assert_eq!(strip_sgr(&c), plain);
        assert!(
            plain.starts_with("         \u{23bf} "),
            "connector aligns with the rows: {plain:?}"
        );
    }

    #[test]
    fn connector_without_annotations_omits_legend() {
        let p = std::path::Path::new("hooks/totals.json");
        let c = render_connector(p, "", "", ColorMode::Plain);
        assert!(!c.contains('-'), "no legend when unannotated: {c:?}");
        assert!(c.ends_with("hooks/totals.json"));
    }
}
