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
