//! Drives `.github/scripts/release-plan.sh` with synthetic `git diff` / `git log`
//! output. The script is pure -- two input files in, key=value lines out -- so
//! the weekly release decision is testable without a repository, a tag, or a
//! network.
#![cfg(unix)]

use std::fs;
use std::process::Command;

use tempfile::TempDir;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.github/scripts/release-plan.sh");

/// Runs the script over the given changed-paths and commit-log text, the way
/// the week's first tick and every `workflow_dispatch` call it: no age, so no
/// cooldown.
/// Returns `(exited zero, stdout)`.
fn plan(version: &str, paths: &str, log: &str) -> (bool, String) {
    plan_aged(version, paths, log, None)
}

/// As `plan`, but passing the fourth argument a retry tick supplies: whole
/// hours since the previous release. `Some("")` is the empty string the
/// workflow passes when it has no age to report.
fn plan_aged(version: &str, paths: &str, log: &str, hours: Option<&str>) -> (bool, String) {
    let dir = TempDir::new().unwrap();
    let paths_file = dir.path().join("paths");
    let log_file = dir.path().join("log");
    fs::write(&paths_file, paths).unwrap();
    fs::write(&log_file, log).unwrap();
    let mut cmd = Command::new("sh");
    cmd.arg(SCRIPT).arg(version).arg(&paths_file).arg(&log_file);
    if let Some(hours) = hours {
        cmd.arg(hours);
    }
    let out = cmd.output().expect("sh is available");
    (out.status.success(), String::from_utf8(out.stdout).unwrap())
}

#[test]
fn a_docs_only_week_releases_nothing() {
    let (ok, out) = plan(
        "0.7.0",
        "docs/superpowers/specs/a-design.md\nREADME.md\ntests/cli_sync.rs\n",
        "docs: explain the thing\ntest: cover the thing\n",
    );
    assert!(ok);
    assert_eq!(out, "release=false\nreason=nothing-shippable\n");
}

#[test]
fn an_empty_range_releases_nothing() {
    let (ok, out) = plan("0.7.0", "", "");
    assert!(ok);
    assert_eq!(out, "release=false\nreason=nothing-shippable\n");
}

#[test]
fn a_feat_touching_src_cuts_a_minor() {
    let (ok, out) = plan("0.7.0", "src/cli/sync.rs\n", "feat(sync): add a flag\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=minor\nversion=0.8.0\ntag=v0.8.0\n");
}

#[test]
fn a_fix_only_week_cuts_a_patch() {
    let (ok, out) = plan(
        "0.7.0",
        "src/api/mod.rs\n",
        "fix(api): retry on 502\nchore: tidy an import\ndocs: note the retry\n",
    );
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.7.1\ntag=v0.7.1\n");
}

#[test]
fn a_desktop_only_week_still_ships() {
    // The desktop app is a release asset. Without `desktop/` in the ship gate
    // these weeks read as "nothing to release" and desktop fixes never ship.
    let (ok, out) = plan("0.7.0", "desktop/lib/main.dart\n", "fix(desktop): sidebar focus\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.7.1\ntag=v0.7.1\n");
}

#[test]
fn the_desktop_lockfile_ships() {
    let (ok, out) = plan("0.7.0", "desktop/rust/Cargo.lock\n", "chore(deps): bump a crate\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.7.1\ntag=v0.7.1\n");
}

#[test]
fn the_embedded_template_ships() {
    let (ok, out) = plan("0.7.0", "templates/gitlab-ci.yml\n", "fix(ci): correct the pin\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.7.1\ntag=v0.7.1\n");
}

#[test]
fn a_breaking_marker_in_a_body_forces_a_minor() {
    // On a 0.x crate a breaking change is a minor bump, so `feat`, `feat!` and
    // `BREAKING CHANGE:` all collapse to the same rule.
    let (ok, out) = plan(
        "0.7.0",
        "src/lib.rs\n",
        "fix(codec): drop the legacy field\n\nBREAKING CHANGE: snapshots written by 0.6 no longer load.\n",
    );
    assert!(ok);
    assert_eq!(out, "release=true\nbump=minor\nversion=0.8.0\ntag=v0.8.0\n");
}

#[test]
fn a_bang_suffixed_type_forces_a_minor() {
    let (ok, out) = plan("0.7.0", "src/lib.rs\n", "fix(cli)!: rename --force\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=minor\nversion=0.8.0\ntag=v0.8.0\n");
}

#[test]
fn a_nested_cargo_manifest_does_not_trip_the_gate() {
    // Cargo.toml / Cargo.lock are anchored at the repo root: a fixture manifest
    // under tests/ ships nothing.
    let (ok, out) = plan(
        "0.7.0",
        "tests/fixtures/Cargo.toml\ndocs/notes/x.md\n",
        "test: add a fixture\n",
    );
    assert!(ok);
    assert_eq!(out, "release=false\nreason=nothing-shippable\n");
}

#[test]
fn a_minor_bump_resets_the_patch_component() {
    let (ok, out) = plan("0.7.3", "src/lib.rs\n", "feat: something new\n");
    assert!(ok);
    assert_eq!(out, "release=true\nbump=minor\nversion=0.8.0\ntag=v0.8.0\n");
}

#[test]
fn a_non_semver_current_version_is_an_error() {
    let (ok, out) = plan("0.7", "src/lib.rs\n", "feat: x\n");
    assert!(!ok, "an unparseable version must fail loudly, not guess");
    assert_eq!(out, "");
}

// ---- Retry-tick cooldown ------------------------------------------------
//
// weekly-release.yaml fires three times a Monday because GitHub documents that
// a scheduled run may be delayed or dropped -- which is exactly what happened
// to the 2026-08-31 06:17 UTC tick. Only the two later ticks pass an age, and
// the cooldown is what stops one of them cutting a second release on top of the
// first tick's.

#[test]
fn a_retry_tick_stays_quiet_once_the_week_has_released() {
    // The 06:17 tick released four hours ago and shippable commits have landed
    // since, so the ship gate alone would say yes -- and a second release the
    // same Monday is exactly what the retries must never cause.
    let (ok, out) = plan_aged("0.8.0", "src/cli/sync.rs\n", "feat: a flag\n", Some("4"));
    assert!(ok, "a suppressed retry is a quiet success, not a red run");
    assert_eq!(out, "release=false\nreason=cooldown\n");
}

#[test]
fn a_retry_tick_releases_when_the_first_tick_never_ran() {
    // The whole point of the retries: a week-old tag means the 06:17 tick was
    // dropped, and this run has to do its job.
    let (ok, out) = plan_aged("0.8.0", "src/cli/sync.rs\n", "feat: a flag\n", Some("168"));
    assert!(ok);
    assert_eq!(out, "release=true\nbump=minor\nversion=0.9.0\ntag=v0.9.0\n");
}

#[test]
fn a_hand_cut_release_days_earlier_does_not_suppress_a_retry() {
    // The case a week-wide cooldown would have got wrong. v0.8.0 was cut by
    // hand on a Thursday; the Monday tick four days later was dropped, and the
    // retry standing in for it still has a release to make.
    let (ok, out) = plan_aged("0.8.0", "src/lib.rs\n", "fix: a fix\n", Some("96"));
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.8.1\ntag=v0.8.1\n");
}

#[test]
fn the_cooldown_clears_at_a_day() {
    // A day, not a week: the three ticks are within eight hours of each other,
    // so anything a day old belongs to an earlier release, not to this week's.
    let (ok, out) = plan_aged("0.8.0", "src/lib.rs\n", "fix: a fix\n", Some("24"));
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.8.1\ntag=v0.8.1\n");

    // Eight hours is the widest gap between the first tick and a retry, so it
    // has to still be inside the window.
    let (ok, out) = plan_aged("0.8.0", "src/lib.rs\n", "fix: a fix\n", Some("8"));
    assert!(ok);
    assert_eq!(out, "release=false\nreason=cooldown\n");
}

#[test]
fn an_empty_age_applies_no_cooldown() {
    // What the workflow passes on the week's first tick and on every
    // workflow_dispatch: the argument is always present, and empty means the
    // gate must behave exactly as it did before the retries existed.
    let (ok, out) = plan_aged("0.8.0", "src/lib.rs\n", "fix: a fix\n", Some(""));
    assert!(ok);
    assert_eq!(out, "release=true\nbump=patch\nversion=0.8.1\ntag=v0.8.1\n");
}

#[test]
fn the_cooldown_outranks_the_ship_gate() {
    // Both reasons are true during a cooldown -- the retry reports the one that
    // says the schedule worked, so a suppressed retry is never mistaken for a
    // week in which nothing shippable landed.
    let (ok, out) = plan_aged("0.8.0", "README.md\n", "docs: a doc\n", Some("4"));
    assert!(ok);
    assert_eq!(out, "release=false\nreason=cooldown\n");
}

#[test]
fn a_non_numeric_age_is_an_error() {
    // Same philosophy as the version check: refuse to guess. A `date` that
    // returned junk must not silently read as "no cooldown" and double-release.
    let (ok, out) = plan_aged("0.8.0", "src/lib.rs\n", "fix: a fix\n", Some("soon"));
    assert!(!ok, "an unparseable age must fail loudly, not guess");
    assert_eq!(out, "");
}

#[test]
fn a_negative_age_is_an_error() {
    // A clock skew that puts the tag in the future must be loud, not a release.
    let (ok, out) = plan_aged("0.8.0", "src/lib.rs\n", "fix: a fix\n", Some("-1"));
    assert!(!ok);
    assert_eq!(out, "");
}
