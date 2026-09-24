//! Every ported scenario must keep both of its wrappers, and the live one
//! must keep its `#[ignore]`.
//!
//! The failure this prevents is silent. A `live_*` wrapper that loses its
//! `#[ignore]` runs under a plain `cargo test`, finds no `RDC_LIVE_*`
//! credentials, early-returns from `LiveConfig::from_env()` — and PASSES,
//! having tested nothing. It also breaks the documented contract that
//! `cargo test --test live -- --ignored` selects exactly the live set
//! (README.md:522).
//!
//! Note what is deliberately NOT enforced here: `worker_threads = 2`.
//! `wiremock` 0.6.5 runs its server on its own `std::thread::spawn`ed thread
//! with its own current-thread runtime, and `Teardown::drop` likewise spawns
//! its own thread, so a forgotten `multi_thread` cannot deadlock a port. It
//! is a consistency convention with the live twin, not a hang waiting to
//! happen — stage 1's final review established this and it must not be
//! re-litigated by a future author "fixing" a port to satisfy a rule that
//! protects nothing.
//!
//! Parsing is deliberately simple and line-based, matching
//! `tests/command_references.rs`: a scenario body is a trimmed line of the
//! shape `async fn <name>(cfg: &LiveConfig)` — or `(cfg: &LiveConfig, ...)`,
//! since a body is free to take more than just `cfg`; see
//! `scenario_core_name` below — where `<name>` is what a `fake_<name>` /
//! `live_<name>` pair must share. A signature rustfmt has wrapped across
//! multiple lines would slip past this — the same tradeoff
//! `command_references.rs` makes for backticked verbs: a narrow rule with
//! few false positives beats a clever one that has more, and every
//! signature in this codebase today is short enough not to wrap.
//!
//! The body is identified by its **signature**, not by a `_core` name
//! suffix. An earlier version of this guard matched only
//! `async fn <name>_core(cfg: &LiveConfig)` and stripped `_core` back off to
//! get `<name>` — which meant a shared body ported under any other name
//! (`push_create_ordering`, `organization_settings_push`) was invisible to
//! both checks below, silently, with only the blanket ignored-count still
//! covering it. Signature-keying fixes that false negative but opens a
//! matching false positive: a helper — not a scenario body — that happens to
//! take exactly `(cfg: &LiveConfig)` will now be treated as one, and the
//! guard will demand `fake_<name>` / `live_<name>` wrappers for it that were
//! never meant to exist. That trade is deliberate and considered the better
//! failure mode: it is loud, it names the offending function, and the fix is
//! obvious (rename the helper's parameter, or give it the wrappers it
//! apparently needs) — unlike the false negative it replaces, which passed
//! silently. See `scenario_core_name` below.

use std::path::{Path, PathBuf};

/// The exact live-binary `--ignored` count this repo's suite reports today:
/// `cargo test --test live -- --ignored --list | grep -c ': test'`. That
/// command is documented at README.md:522; the count itself is not — it is
/// derived from this repository's current scenario set, not read out of
/// README. Bump this only alongside a real change to that count (a scenario
/// added or removed), and re-verify with that same command — never by
/// guessing.
const EXPECTED_IGNORED_LIVE_TESTS: usize = 27;

/// `tests/live/scenarios/*.rs`, direct children only — matches how
/// `tests/live.rs` wires scenario modules in one by one.
fn scenario_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live/scenarios");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
        .collect();
    out.sort();
    out
}

/// If a trimmed line is `async fn <name>(cfg: &LiveConfig` followed by `)`
/// (no further parameters) or `,` (more follow), return `<name>` verbatim
/// (no suffix is stripped — `<name>` may or may not end in `_core`; that is
/// just whatever the author called the body). Only the shared body's first
/// parameter is `cfg: &LiveConfig`; both wrappers take no arguments at all,
/// which is what keeps this from matching them.
///
/// The `)`-or-`,` boundary matters: a scenario body threading a second
/// parameter through (`(cfg: &LiveConfig, seed: &Seed)`) must still be
/// recognized, or it reproduces — merely in a new disguise — the exact false
/// negative this guard was already fixed once for: a scenario body invisible
/// to both wrapper checks below, silently, until it had already let two real
/// ports through unported. Requiring the boundary character (rather than a
/// bare `starts_with("(cfg: &LiveConfig")`) also stops a type that merely
/// starts with `LiveConfig` — some future `LiveConfigExtra`, say — from being
/// mistaken for the real type.
///
/// This keys on the signature, not the name, on purpose: it is what lets the
/// guard see `push_create_ordering` and `organization_settings_push`
/// alongside `round_trip_core`, none of which share a name suffix. The
/// tradeoff — a non-scenario helper that happens to take `cfg: &LiveConfig`
/// as its first parameter will be mistaken for a scenario body and the guard
/// will demand wrappers it doesn't have — is deliberate; see the module doc
/// comment above for why that failure mode is the one worth accepting.
fn scenario_core_name(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("async fn ")?;
    let paren = rest.find('(')?;
    let name = rest[..paren].trim();
    let after_cfg = rest[paren..].strip_prefix("(cfg: &LiveConfig")?;
    if name.is_empty() || !after_cfg.starts_with([')', ',']) {
        return None;
    }
    Some(name.to_string())
}

/// True if some line of `text`, trimmed, starts with `async fn <name>(` —
/// i.e. a function literally named `name` is defined. The trailing `(` stops
/// a longer name that merely starts with `name` (a future
/// `fake_round_trip_core_v2`, say) from matching.
fn defines_fn(text: &str, name: &str) -> bool {
    let needle = format!("async fn {name}(");
    text.lines().any(|l| l.trim_start().starts_with(&needle))
}

/// True if some line of `text` defines `async fn <name>(...)` AND the line
/// immediately above it, trimmed, starts with `#[ignore`. Operates on a
/// single file's text so "immediately above" never crosses a file boundary.
fn defines_ignored_fn(text: &str, name: &str) -> bool {
    let needle = format!("async fn {name}(");
    let lines: Vec<&str> = text.lines().collect();
    lines.iter().enumerate().any(|(i, l)| {
        l.trim_start().starts_with(&needle)
            && i > 0
            && lines[i - 1].trim_start().starts_with("#[ignore")
    })
}

/// The lines strictly between `fn_name`'s `async fn <fn_name>(...)`
/// signature line and the first following line that is exactly `}` with no
/// indentation — i.e. the function's body, signature and closing brace both
/// excluded. Every wrapper in this file is a top-level function, so a
/// bare, unindented `}` is a reliable terminator: it cannot be a nested
/// block's closing brace, which always carries at least one level of
/// indentation. Returns `None` if `fn_name`'s signature (or, having found
/// it, a matching terminator) is not present in `text`.
fn fn_body<'a>(text: &'a str, fn_name: &str) -> Option<Vec<&'a str>> {
    let needle = format!("async fn {fn_name}(");
    let lines: Vec<&str> = text.lines().collect();
    let sig = lines.iter().position(|l| l.trim_start().starts_with(&needle))?;
    let end = lines[sig + 1..].iter().position(|l| *l == "}")?;
    Some(lines[sig + 1..sig + 1 + end].to_vec())
}

/// The part of `line` before its first `//` — a naive line-comment strip,
/// with no notion of a `//` inside a string literal (no line in this file's
/// subject matter has ever needed one). Used by `wrapper_calls_body_name` so
/// a call left in only as a dead, commented-out line does not count as the
/// wrapper actually calling it.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// True if `wrapper_name`'s body (see `fn_body`) contains a call to
/// `target_name` — the literal substring `<target_name>(`, with the
/// character immediately before it (if any) not an identifier character
/// (`_` or alphanumeric), so a longer name merely ENDING in `target_name`
/// (a `big_target_name(` call) cannot satisfy it.
///
/// Deliberately excludes the signature line from the search: `wrapper_name`
/// is conventionally `fake_<target_name>`, so the signature line itself —
/// `async fn fake_<target_name>(...)` — already contains the substring
/// `<target_name>(` as its own tail. A check that searched the signature
/// line too would be satisfied by every wrapper unconditionally, including
/// one with a completely empty body, which is the exact failure mode this
/// check exists to catch: an empty or stubbed `fake_X` wrapper that
/// satisfies the pairing check in `every_scenario_core_has_both_wrappers`
/// and shows up as a green test that ran nothing.
///
/// Also strips each body line's `//` line comment before searching (see
/// `strip_line_comment`) — a "stubbed" wrapper is exactly as likely to have
/// its call commented out as deleted outright (this file's own sabotage
/// history bears that out: the first draft of this check was verified
/// against a wrapper with its call deleted, and separately still passed a
/// wrapper whose call was merely commented out, which is equally "ran
/// nothing"). Naive and line-based, like every other check in this file —
/// it does not know about `//` inside a string literal, which no scenario
/// wrapper in this codebase has ever needed.
fn wrapper_calls_body_name(text: &str, wrapper_name: &str, target_name: &str) -> bool {
    let Some(body_lines) = fn_body(text, wrapper_name) else {
        return false;
    };
    let body: String =
        body_lines.iter().map(|l| strip_line_comment(l)).collect::<Vec<_>>().join("\n");
    let needle = format!("{target_name}(");
    let mut from = 0usize;
    while let Some(rel) = body[from..].find(&needle) {
        let at = from + rel;
        let prev_is_ident = body[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if !prev_is_ident {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Count of lines that, trimmed, literally start with `#[ignore` — real
/// attribute syntax, not a doc-comment mention. `round_trip.rs` carries one
/// of the latter on purpose ("Unchanged: same `#[ignore]`..."); a naive
/// `contains("#[ignore")` scan would count it alongside the real attribute
/// two lines below it. (`ignore_attr_count_ignores_a_doc_comment_mention`
/// below pins this against that exact line.)
fn ignore_attr_count(text: &str) -> usize {
    text.lines()
        .filter(|l| l.trim_start().starts_with("#[ignore"))
        .count()
}

#[test]
fn every_scenario_core_has_both_wrappers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = scenario_files();
    assert!(
        files.len() >= 10,
        "found only {} scenario file(s) under tests/live/scenarios; \
         has that directory moved?",
        files.len()
    );

    let sources: Vec<(PathBuf, String)> = files
        .iter()
        .map(|f| {
            let text = std::fs::read_to_string(f)
                .unwrap_or_else(|e| panic!("read {}: {e}", f.display()));
            (f.clone(), text)
        })
        .collect();

    let mut cores = Vec::new();
    for (f, text) in &sources {
        for (n, line) in text.lines().enumerate() {
            if let Some(name) = scenario_core_name(line) {
                cores.push((name, f.clone(), n + 1));
            }
        }
    }
    assert!(
        !cores.is_empty(),
        "found zero `async fn <name>(cfg: &LiveConfig)` scenario bodies; \
         has round_trip.rs changed shape, or has the parser broken?"
    );

    let mut bad = Vec::new();
    let mut ported_live_ignored = 0usize;
    for (name, file, line) in &cores {
        let rel = file.strip_prefix(root).unwrap_or(file);
        let fake_name = format!("fake_{name}");
        let live_name = format!("live_{name}");

        match sources.iter().find(|(_, t)| defines_fn(t, &fake_name)) {
            None => {
                bad.push(format!(
                    "{}:{line}: `{name}` has no `{fake_name}` wrapper",
                    rel.display()
                ));
            }
            Some((_, t)) if !wrapper_calls_body_name(t, &fake_name, name) => {
                bad.push(format!(
                    "{}:{line}: `{fake_name}` is defined but its body never \
                     mentions `{name}` — an empty or stubbed wrapper would \
                     satisfy the pairing check above and run nothing",
                    rel.display()
                ));
            }
            Some(_) => {}
        }

        let live_ignored = sources.iter().any(|(_, t)| defines_ignored_fn(t, &live_name));
        let live_exists = sources.iter().any(|(_, t)| defines_fn(t, &live_name));
        if live_ignored {
            ported_live_ignored += 1;
        } else if live_exists {
            bad.push(format!(
                "{}:{line}: `{live_name}` exists but `#[ignore]` is not the \
                 line immediately above its `async fn` — a plain `cargo \
                 test` would run it, find no RDC_LIVE_* credentials, and \
                 have it silently pass having tested nothing",
                rel.display()
            ));
        } else {
            bad.push(format!(
                "{}:{line}: `{name}` has no `{live_name}` wrapper",
                rel.display()
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "scenario-wrapper contract violated:\n{}",
        bad.join("\n")
    );

    let total_ignored: usize = sources.iter().map(|(_, t)| ignore_attr_count(t)).sum();
    let unported_ignored = total_ignored.saturating_sub(ported_live_ignored);
    assert_eq!(
        total_ignored, EXPECTED_IGNORED_LIVE_TESTS,
        "{ported_live_ignored} ported live wrapper(s) + {unported_ignored} \
         not-yet-ported ignored scenario(s) = {total_ignored} ignored \
         test(s) under tests/live/scenarios, want \
         {EXPECTED_IGNORED_LIVE_TESTS} (what `cargo test --test live -- \
         --ignored --list | grep -c ': test'` reports; that command is \
         documented at README.md:522, the count itself is not — it is \
         derived from this repo's current scenario set). If this changed \
         on purpose (a scenario added or removed), update \
         EXPECTED_IGNORED_LIVE_TESTS to match and re-verify with that \
         same command."
    );
}

#[test]
fn scenario_core_name_matches_only_the_shared_body() {
    // A name ending in `_core` still matches — but on the signature, not the
    // suffix; nothing strips `_core` off any more.
    assert_eq!(
        scenario_core_name("async fn round_trip_core(cfg: &LiveConfig) {"),
        Some("round_trip_core".to_string())
    );
    assert_eq!(
        scenario_core_name("    async fn round_trip_core(cfg: &LiveConfig) {"),
        Some("round_trip_core".to_string()),
        "leading indentation must not matter"
    );
    // Names with no `_core` suffix at all must match just as well — these
    // are the two cases the old suffix-keyed rule silently missed.
    assert_eq!(
        scenario_core_name("async fn push_create_ordering(cfg: &LiveConfig) {"),
        Some("push_create_ordering".to_string())
    );
    assert_eq!(
        scenario_core_name("async fn organization_settings_push(cfg: &LiveConfig) {"),
        Some("organization_settings_push".to_string())
    );
    // The wrappers take no arguments at all — that must NOT match.
    assert_eq!(
        scenario_core_name("async fn fake_round_trip_core() {"),
        None
    );
    assert_eq!(
        scenario_core_name("async fn live_round_trip_core() {"),
        None
    );
    // A helper with an unrelated signature.
    assert_eq!(
        scenario_core_name(
            "async fn remote_color(client: &LiveClient, id: u64) -> String {"
        ),
        None
    );
    // Prose mentioning the shape in a doc comment must not match either.
    assert_eq!(
        scenario_core_name("/// calls round_trip_core(cfg: &LiveConfig) internally"),
        None
    );
}

#[test]
fn scenario_core_name_matches_a_body_that_takes_more_than_cfg() {
    // A future scenario body threading a second parameter through
    // (`(cfg: &LiveConfig, seed: &Seed)`) must still be recognized — the
    // exact false negative this guard was already fixed once for (a body
    // invisible to both wrapper checks), in a new disguise.
    assert_eq!(
        scenario_core_name("async fn seeded_core(cfg: &LiveConfig, seed: &Seed) {"),
        Some("seeded_core".to_string())
    );
    assert_eq!(
        scenario_core_name("async fn seeded_core(cfg: &LiveConfig, seed: &Seed) -> Result<()> {"),
        Some("seeded_core".to_string())
    );
    // A type name that merely starts with `LiveConfig` (not the type itself)
    // must not match — the comma/paren boundary check exists precisely to
    // rule this out.
    assert_eq!(
        scenario_core_name("async fn odd_core(cfg: &LiveConfigExtra) {"),
        None
    );
}

#[test]
fn ignore_attr_count_ignores_a_doc_comment_mention() {
    // The real doc comment from round_trip.rs's live wrapper, verbatim. A
    // naive `contains("#[ignore")` scan double-counts it alongside the real
    // attribute two lines below.
    let text = "/// The live twin. Unchanged: same `#[ignore]`, same env gate, so\n\
                 #[tokio::test(flavor = \"multi_thread\", worker_threads = 2)]\n\
                 #[ignore = \"live: needs RDC_LIVE_* env\"]\n\
                 async fn live_round_trip_core() {}\n";
    assert_eq!(ignore_attr_count(text), 1);
}

#[test]
fn defines_ignored_fn_requires_the_attribute_directly_above() {
    let paired = "#[ignore = \"live: needs RDC_LIVE_* env\"]\n\
                  async fn live_x_core() {}\n";
    assert!(defines_ignored_fn(paired, "live_x_core"));

    // A blank line (or anything else) between the attribute and the fn
    // breaks the pairing this guard requires.
    let separated = "#[ignore = \"live: needs RDC_LIVE_* env\"]\n\
                      \n\
                      async fn live_x_core() {}\n";
    assert!(!defines_ignored_fn(separated, "live_x_core"));

    let unignored = "async fn live_x_core() {}\n";
    assert!(!defines_ignored_fn(unignored, "live_x_core"));
}

#[test]
fn wrapper_calls_body_name_is_false_for_an_empty_wrapper_body() {
    // The trap: `fake_round_trip_core`'s own SIGNATURE line contains the
    // substring `round_trip_core(` (as the tail of `fake_round_trip_core(`),
    // so a naive `full_text.contains(&format!("{name}("))` scan would report
    // `true` here even though the body between the braces is empty — the
    // exact failure mode (a green check that tests nothing) this helper
    // exists to catch. If this assertion passed against that naive scan, the
    // check would be tautological.
    let text = "async fn fake_round_trip_core() {\n}\n";
    assert!(!wrapper_calls_body_name(text, "fake_round_trip_core", "round_trip_core"));
}

#[test]
fn wrapper_calls_body_name_is_false_for_a_stubbed_body() {
    let text = "async fn fake_round_trip_core() {\n    // TODO: wire this up\n}\n";
    assert!(!wrapper_calls_body_name(text, "fake_round_trip_core", "round_trip_core"));
}

#[test]
fn wrapper_calls_body_name_is_true_when_the_body_calls_it() {
    let text = "async fn fake_round_trip_core() {\n\
                 \x20\x20\x20\x20let fake = crate::support::fake::FakeOrg::start().await;\n\
                 \x20\x20\x20\x20round_trip_core(&fake.config()).await;\n\
                 }\n";
    assert!(wrapper_calls_body_name(text, "fake_round_trip_core", "round_trip_core"));
}

#[test]
fn wrapper_calls_body_name_ignores_a_commented_out_call() {
    // A call left in only as a `//` comment — stubbed out, e.g. mid-refactor
    // — must not count: the wrapper calls nothing at runtime, which is
    // exactly the "ran nothing" failure mode this check exists to catch.
    let text = "async fn fake_round_trip_core() {\n    // round_trip_core(&fake.config()).await;\n}\n";
    assert!(!wrapper_calls_body_name(text, "fake_round_trip_core", "round_trip_core"));
}

#[test]
fn wrapper_calls_body_name_rejects_a_longer_identifier_ending_in_the_name() {
    // `big_round_trip_core(` ends in `round_trip_core(` too, but the
    // character right before the match (`g`) is an identifier character, so
    // this must not count as a call to `round_trip_core`.
    let text = "async fn fake_round_trip_core() {\n    big_round_trip_core(&cfg).await;\n}\n";
    assert!(!wrapper_calls_body_name(text, "fake_round_trip_core", "round_trip_core"));
}

#[test]
fn fn_body_excludes_the_signature_and_closing_brace_lines() {
    let text = "async fn fake_x() {\n    one();\n    two();\n}\n";
    assert_eq!(fn_body(text, "fake_x"), Some(vec!["    one();", "    two();"]));
}

#[test]
fn fn_body_is_none_when_the_signature_is_missing() {
    assert_eq!(fn_body("async fn fake_y() {\n}\n", "fake_x"), None);
}
