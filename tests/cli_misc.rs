//! Pure-clap parse tests for cross-flag rejections that don't merit a
//! per-subcommand integration file. These run without spawning the
//! binary — they exercise the `Cli::try_parse_from` API directly so
//! failures point straight at the `#[arg(...)]` configuration.
//!
//! Add tests here when the only thing under test is clap's
//! conflict / requires graph, not end-to-end behavior.

use clap::Parser;

#[test]
fn sync_watch_and_dry_run_are_mutually_exclusive() {
    let result = rdc::cli::Cli::try_parse_from(["rdc", "sync", "test", "--watch", "--dry-run"]);
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("--watch") || err.contains("--dry-run"), "{err}");
}

#[test]
fn sync_poll_interval_requires_watch() {
    let result =
        rdc::cli::Cli::try_parse_from(["rdc", "sync", "test", "--poll-interval", "30s"]);
    assert!(
        result.is_err(),
        "should reject --poll-interval without --watch"
    );
}

#[test]
fn sync_watch_accepts_poll_interval() {
    let cli = rdc::cli::Cli::try_parse_from([
        "rdc",
        "sync",
        "test",
        "--watch",
        "--poll-interval",
        "30s",
    ])
    .expect("valid CLI");
    if let Some(rdc::cli::Command::Sync { poll_interval, watch, .. }) = cli.command {
        assert_eq!(poll_interval, "30s");
        assert!(watch);
    } else {
        panic!("expected Sync variant");
    }
}

#[test]
fn migrate_carry_accepts_a_single_group() {
    let cli = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--carry", "automation",
    ]);
    assert!(cli.is_ok(), "--carry automation must parse: {:?}", cli.err());
}

#[test]
fn migrate_carry_accepts_comma_separated_groups() {
    let cli = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--carry", "score-thresholds,automation",
    ]);
    assert!(
        cli.is_ok(),
        "one --carry may name several groups: {:?}",
        cli.err()
    );
}

#[test]
fn migrate_carry_accepts_a_repeated_flag() {
    let cli = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--carry", "score-thresholds", "--carry", "automation",
    ]);
    assert!(cli.is_ok(), "--carry must be repeatable: {:?}", cli.err());
}

#[test]
fn migrate_carry_rejects_an_unknown_group() {
    // Names the valid set for the reader rather than failing anonymously —
    // clap's ValueEnum error does this for free, which is why the option is a
    // value enum instead of a hand-parsed string.
    let err = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--carry", "thresholds",
    ])
    .expect_err("an unknown group must be rejected");
    let msg = format!("{err}");
    assert!(msg.contains("score-thresholds"), "{msg}");
}

/// `--migrate-score-thresholds` was removed in favour of `--carry
/// score-thresholds`. A pipeline that bumps `RDC_VERSION` and still passes it
/// must fail at argument parse — before any file is written — not silently
/// migrate thresholds it meant to keep.
#[test]
fn migrate_rejects_the_removed_score_threshold_flag() {
    let result = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--migrate-score-thresholds",
    ]);
    assert!(result.is_err(), "the removed flag must not parse");
}

/// The email-prefix half of the same removal.
#[test]
fn migrate_rejects_the_removed_email_prefix_flag() {
    let result = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--migrate-email-prefixes",
    ]);
    assert!(result.is_err(), "the removed flag must not parse");
}

#[test]
fn migrate_carry_parses_into_the_named_groups() {
    let cli = rdc::cli::Cli::try_parse_from([
        "rdc", "migrate", "test", "prod", "--carry", "score-thresholds,automation",
    ])
    .expect("valid CLI");
    let Some(rdc::cli::Command::Migrate { carry, .. }) = cli.command else {
        panic!("expected Migrate variant");
    };
    assert_eq!(
        rdc::cli::migrate::Carry::from_groups(&carry),
        rdc::cli::migrate::Carry {
            score_thresholds: true,
            email_prefixes: false,
            automation: true,
        }
    );
}

// ---------------------------------------------------------------------------
// Verb surface: abbreviations, and the absence of hidden verbs.
// ---------------------------------------------------------------------------

/// Which verb a command line reaches, resolved through the real
/// `Cli::command()` tree rather than the `Command` enum, so these stay honest
/// when variants move.
fn resolve(argv: &[&str]) -> String {
    use clap::CommandFactory;
    rdc::cli::Cli::command()
        .try_get_matches_from(argv)
        .map(|m| m.subcommand_name().unwrap_or("<no verb>").to_string())
        .unwrap_or_else(|e| format!("<{:?}>", e.kind()))
}

/// `rdc i` is `rdc init`: clap's `infer_subcommands` accepts any unambiguous
/// prefix. The full name always works too -- clap falls back to an exact
/// match, so no spelling that worked before resolves anywhere new.
#[test]
fn an_unambiguous_prefix_resolves_to_its_verb() {
    for (typed, verb) in [
        ("i", "init"),
        ("in", "init"),
        ("ini", "init"),
        ("init", "init"),
        ("s", "sync"),
        ("sy", "sync"),
        ("sync", "sync"),
        ("m", "migrate"),
        ("a", "auth"),
        ("d", "doctor"),
        ("do", "doctor"),
        ("u", "upgrade"),
        ("up", "upgrade"),
    ] {
        assert_eq!(
            resolve(&["rdc", typed]),
            verb,
            "`rdc {typed}` should reach `rdc {verb}`"
        );
    }
}

/// Inference matches prefixes, not fuzzy spellings: a string that prefixes no
/// verb is still an error rather than a guess at what was meant.
#[test]
fn a_prefix_of_nothing_is_still_rejected() {
    assert_eq!(resolve(&["rdc", "x"]), "<InvalidSubcommand>");
    assert_eq!(resolve(&["rdc", "snyc"]), "<InvalidSubcommand>");
}

/// No verb may be hidden from the help output. A hidden verb is
/// undiscoverable by definition, and under `infer_subcommands` it quietly
/// eats a prefix as well: while the `deploy` shim existed, `rdc d` was
/// ambiguous with `doctor` and so resolved to neither.
#[test]
fn the_cli_exposes_no_hidden_verbs() {
    use clap::CommandFactory;
    let cmd = rdc::cli::Cli::command();
    let hidden: Vec<String> = cmd
        .get_subcommands()
        .filter(|s| s.is_hide_set())
        .map(|s| s.get_name().to_string())
        .collect();
    assert!(
        hidden.is_empty(),
        "hidden verb(s): {hidden:?}. Every verb `rdc` accepts must appear in \
         its help output; retire a command outright rather than hiding it."
    );
}

/// Single-letter abbreviations are a promise the next verb can break: adding
/// `status` would turn a working `rdc s` into "unrecognized subcommand". Keep
/// first letters distinct, or add the verb knowing what it costs.
#[test]
fn every_verb_starts_with_a_distinct_letter() {
    use clap::CommandFactory;
    use std::collections::BTreeMap;
    let cmd = rdc::cli::Cli::command();
    let mut by_letter: BTreeMap<char, Vec<String>> = BTreeMap::new();
    for sub in cmd.get_subcommands() {
        let name = sub.get_name().to_string();
        let Some(first) = name.chars().next() else {
            continue;
        };
        by_letter.entry(first).or_default().push(name);
    }
    let clashes: Vec<String> = by_letter
        .iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(letter, names)| format!("{letter}: {}", names.join(", ")))
        .collect();
    assert!(
        clashes.is_empty(),
        "verbs sharing a first letter kill that single-letter abbreviation \
         for everyone who already types it -- {}",
        clashes.join("; ")
    );
}

/// Every subcommand and every argument must carry help text.
///
/// Written after `rdc init --env <ENV_SPEC>`, `rdc sync [ENV]`, `rdc auth
/// --token/--username` and `rdc doctor [ENV]` all shipped rendering a blank
/// help column. Nothing breaks when that happens — the flag parses fine — so
/// the only reader who notices is the one trying to work out what to pass,
/// and for `--env` the grammar `<env>=<api_base>:<org_id>` was guessable from
/// nowhere but the source.
///
/// Positionals are included deliberately: a bare `[ENV]` with no help is the
/// case that actually occurred, four times over.
#[test]
fn every_arg_and_subcommand_has_help() {
    use clap::CommandFactory;
    let cmd = rdc::cli::Cli::command();
    let mut missing: Vec<String> = Vec::new();

    fn visit(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
        for arg in cmd.get_arguments() {
            // `-h/--help` and `-V/--version` are clap's own; it supplies their
            // text and the derive has nowhere to put ours.
            if matches!(arg.get_id().as_str(), "help" | "version") {
                continue;
            }
            let has_help = arg
                .get_help()
                .or_else(|| arg.get_long_help())
                .is_some_and(|h| !h.to_string().trim().is_empty());
            if !has_help {
                missing.push(format!("{path} {}", arg.get_id()));
            }
        }
        for sub in cmd.get_subcommands() {
            let has_about = sub
                .get_about()
                .or_else(|| sub.get_long_about())
                .is_some_and(|h| !h.to_string().trim().is_empty());
            if !has_about {
                missing.push(format!("{path} {} (subcommand)", sub.get_name()));
            }
            visit(sub, &format!("{path} {}", sub.get_name()), missing);
        }
    }
    visit(&cmd, "rdc", &mut missing);

    assert!(
        missing.is_empty(),
        "these render a blank help column, leaving a reader to guess what to \
         pass -- {}",
        missing.join("; ")
    );
}
