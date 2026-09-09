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
//! `tests/command_references.rs`: a scenario "core" is a trimmed line of the
//! shape `async fn <name>_core(cfg: &LiveConfig)`; `<name>` is what a
//! `fake_<name>_core` / `live_<name>_core` pair must share. A signature
//! rustfmt has wrapped across multiple lines would slip past this — the same
//! tradeoff `command_references.rs` makes for backticked verbs: a narrow rule
//! with no false positives beats a clever one that has them, and every
//! signature in this codebase today is short enough not to wrap.

use std::path::{Path, PathBuf};

/// The exact live-binary `--ignored` count this repo's suite reports today:
/// `cargo test --test live -- --ignored --list | grep -c ': test'`,
/// documented at README.md:522. Bump this only alongside a real change to
/// that count (a scenario added or removed), and re-verify with that same
/// command — never by guessing.
const EXPECTED_IGNORED_LIVE_TESTS: usize = 23;

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

/// If a trimmed line is `async fn <name>_core(cfg: &LiveConfig)`, return
/// `<name>`. Only the shared body takes `cfg: &LiveConfig`; both wrappers
/// take no arguments at all, which is what keeps this from matching them.
fn scenario_core_name(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("async fn ")?;
    let paren = rest.find('(')?;
    let name = rest[..paren].trim();
    if !rest[paren..].starts_with("(cfg: &LiveConfig)") {
        return None;
    }
    name.strip_suffix("_core").map(str::to_string)
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
        "found zero `async fn <name>_core(cfg: &LiveConfig)` scenario bodies; \
         has round_trip.rs changed shape, or has the parser broken?"
    );

    let mut bad = Vec::new();
    let mut ported_live_ignored = 0usize;
    for (name, file, line) in &cores {
        let rel = file.strip_prefix(root).unwrap_or(file);
        let fake_name = format!("fake_{name}_core");
        let live_name = format!("live_{name}_core");

        if !sources.iter().any(|(_, t)| defines_fn(t, &fake_name)) {
            bad.push(format!(
                "{}:{line}: `{name}_core` has no `{fake_name}` wrapper",
                rel.display()
            ));
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
                "{}:{line}: `{name}_core` has no `{live_name}` wrapper",
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
         --ignored --list | grep -c ': test'` reports, documented at \
         README.md:522). If this changed on purpose (a scenario added or \
         removed), update EXPECTED_IGNORED_LIVE_TESTS and re-verify with \
         that command."
    );
}

#[test]
fn scenario_core_name_matches_only_the_shared_body() {
    assert_eq!(
        scenario_core_name("async fn round_trip_core(cfg: &LiveConfig) {"),
        Some("round_trip".to_string())
    );
    assert_eq!(
        scenario_core_name("    async fn round_trip_core(cfg: &LiveConfig) {"),
        Some("round_trip".to_string()),
        "leading indentation must not matter"
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
