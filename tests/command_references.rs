//! Every ``rdc <verb>`` this repo prints or documents must name a verb the
//! binary actually accepts.
//!
//! Written after a `sync` conflict summary spent months telling users that
//! "committing the local tombstone needs an explicit `rdc push
//! --allow-deletes <env>` follow-up". There has never been an `rdc push`:
//! push is a *phase* of `rdc sync`, and the message was copied verbatim out
//! of the design plan that introduced the conflict resolver. The advice was
//! otherwise correct — a retained lockfile entry with no local file becomes a
//! tombstone, which `rdc sync <env> --allow-deletes` commits — so nothing
//! failed and no test noticed; the user simply had nowhere to go.
//!
//! That is the failure mode this guards: not a crash, but a dead end handed
//! to someone following instructions. Commands come and go here (`deploy`
//! became `migrate` + `sync`; `doctor --rebuild-lock` was dropped), and every
//! removal leaves prose behind that still compiles.
//!
//! The rule is deliberately narrow so it has no false positives: a backtick
//! IMMEDIATELY before `rdc` marks a command, and this codebase writes plain
//! prose ("rdc never writes…", "rdc manages shared views only") without one.
//! The verb is then checked against clap itself — not a hand-kept list — so
//! removing a subcommand fails this test until the prose catches up. There is
//! no hidden-verb exception to carve out: `the_cli_exposes_no_hidden_verbs`
//! (tests/cli_misc.rs) pins that every verb clap accepts is one that
//! `rdc --help` lists, so what a reader is pointed at, they can also find.
//!
//! `docs/superpowers/` and `.superpowers/` are exempt. They are dated design
//! records — plans and specs describing what was true when they were written,
//! including the plan that seeded the bug above. Rewriting them would falsify
//! the history that explains today's code.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Verbs the binary accepts, straight from the clap definition, aliases
/// included.
fn real_verbs() -> BTreeSet<String> {
    use clap::CommandFactory;
    let cmd = rdc::cli::Cli::command();
    let mut out = BTreeSet::new();
    for sub in cmd.get_subcommands() {
        out.insert(sub.get_name().to_string());
        for alias in sub.get_all_aliases() {
            out.insert(alias.to_string());
        }
    }
    // `--help`/`--version` reach the user as `rdc --help`, never as a verb,
    // so nothing else belongs here.
    assert!(
        out.contains("sync") && out.contains("migrate"),
        "clap introspection returned no recognizable subcommands: {out:?}"
    );
    out
}

/// Can a reader actually run `rdc <token>`? Answered by the real parser
/// rather than by matching names, so exact verbs, aliases and -- since
/// `infer_subcommands` -- every unambiguous prefix count automatically, and
/// this stays right the day any of those change.
fn reachable(token: &str) -> bool {
    use clap::CommandFactory;
    use clap::error::ErrorKind;
    match rdc::cli::Cli::command().try_get_matches_from(["rdc", token]) {
        Ok(m) => m.subcommand_name().is_some(),
        // The token resolved and clap stopped for an unrelated reason: it
        // printed help or the version (`rdc --help` is a fine thing to point
        // a reader at), or the line is a fragment missing an argument that
        // the prose goes on to supply.
        Err(e) => matches!(
            e.kind(),
            ErrorKind::DisplayHelp
                | ErrorKind::DisplayVersion
                | ErrorKind::MissingRequiredArgument
        ),
    }
}

/// `infer_subcommands` makes an unambiguous prefix a real invocation, so prose
/// may legitimately write `rdc i`. A guard that only knew full names would
/// reject a line that works.
#[test]
fn an_abbreviated_command_counts_as_documented() {
    for token in ["i", "in", "s", "d", "do", "u", "sync", "migrate"] {
        assert!(reachable(token), "`rdc {token}` runs today; the guard must accept it");
    }
}

/// A prefix of nothing is still a dead end -- the whole point of the guard.
#[test]
fn an_unrunnable_command_is_still_caught() {
    for token in ["push", "pull", "deploy", "x"] {
        assert!(!reachable(token), "`rdc {token}` does not run; the guard must catch it");
    }
}

/// `rdc --help` is something a reader can be pointed at, even though it is
/// not a verb. The extractor reads it as one, so the check must not choke.
#[test]
fn the_help_and_version_flags_are_reachable() {
    assert!(reachable("--help"));
    assert!(reachable("--version"));
}

/// Trees that ship or run: anything a user or a future maintainer reads as
/// current fact. Paths are relative to the crate root.
const SCANNED: &[&str] = &[
    "src",
    "tests",
    "templates",
    "desktop/lib",
    "desktop/rust/src",
    "desktop/README.md",
    "README.md",
    "CLAUDE.md",
];

/// Dated design records — see the module header.
const EXEMPT_DIRS: &[&str] = &["docs", ".superpowers", "target", "build", "node_modules"];

const SCANNED_EXTS: &[&str] = &["rs", "md", "yml", "yaml", "dart", "py", "toml", "sh"];

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if SCANNED_EXTS.contains(&ext) {
            out.push(path.to_path_buf());
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if p.is_dir() && (EXEMPT_DIRS.contains(&name.as_str()) || name.starts_with('.')) {
            continue;
        }
        collect(&p, out);
    }
}

/// Every `` `rdc <verb>` `` occurrence in `text`, as `(line_number, verb)`.
///
/// A trailing character that is alphanumeric disqualifies the match, which is
/// what keeps version strings (`` `rdc v0.9.0` ``, `` `rdc vX.Y.Z` ``) out:
/// the verb would be a bare `v` followed by a digit or capital.
fn backticked_verbs(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut from = 0;
        while let Some(rel) = line[from..].find("`rdc ") {
            let start = from + rel;
            from = start + 5;
            let mut end = from;
            while end < bytes.len() && (bytes[end].is_ascii_lowercase() || bytes[end] == b'-') {
                end += 1;
            }
            if end == from {
                continue; // `rdc ` followed by something that is not a verb
            }
            let next_is_alnum = bytes.get(end).is_some_and(|c| c.is_ascii_alphanumeric());
            if next_is_alnum {
                continue; // `rdc v0.9.0`, `rdc vX.Y.Z` — a version, not a verb
            }
            out.push((n + 1, line[from..end].to_string()));
        }
    }
    out
}

#[test]
fn every_documented_rdc_command_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let verbs = real_verbs();

    let mut files = Vec::new();
    for entry in SCANNED {
        collect(&root.join(entry), &mut files);
    }
    assert!(
        files.len() > 50,
        "the scan found only {} file(s); SCANNED paths have probably moved",
        files.len()
    );

    // This very file names the bad command in its own header, on purpose.
    let self_path = root.join("tests/command_references.rs");

    let mut bad = Vec::new();
    for f in &files {
        if *f == self_path {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(f) else {
            continue; // not UTF-8; nothing to read
        };
        for (line, verb) in backticked_verbs(&text) {
            if !reachable(&verb) {
                let rel = f.strip_prefix(root).unwrap_or(f);
                bad.push(format!("{}:{line}  `rdc {verb}`", rel.display()));
            }
        }
    }

    assert!(
        bad.is_empty(),
        "{} reference(s) to an rdc command that does not exist.\n\
         The binary accepts: {}.\n\
         A push or pull is a PHASE of `rdc sync`, not a command — say so, or \
         name the command a reader can actually run.\n{}",
        bad.len(),
        verbs.iter().cloned().collect::<Vec<_>>().join(", "),
        bad.join("\n"),
    );
}

/// The scan is worthless if the extractor cannot see a command, so pin both
/// directions on strings taken from the real tree.
#[test]
fn extractor_finds_commands_and_ignores_prose() {
    let found = backticked_verbs(
        "run `rdc sync <env> --allow-deletes` then `rdc migrate`\n\
         rdc never writes it, and `rdc://queues/x` is a ref\n\
         rdc v0.9.0 is available; run `rdc upgrade` to install\n\
         Annotated, body `rdc vX.Y.Z` -- the shape every tag has\n\
         `rdc doctor <env> --dry-run`",
    );
    let verbs: Vec<&str> = found.iter().map(|(_, v)| v.as_str()).collect();
    assert_eq!(
        verbs,
        vec!["sync", "migrate", "upgrade", "doctor"],
        "extractor must catch every backticked command and nothing else: {found:?}"
    );
}

/// A removed command is caught wherever it hides — including a doc comment,
/// which is where all of them were.
#[test]
fn extractor_catches_a_command_in_a_doc_comment() {
    let found = backticked_verbs("//! Delete phase for `rdc push`: turn tombstones into DELETEs.");
    assert_eq!(found, vec![(1, "push".to_string())]);
    assert!(
        !real_verbs().contains("push"),
        "there is no `rdc push`; if one is ever added, this test's premise changes"
    );
}
