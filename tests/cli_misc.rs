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
