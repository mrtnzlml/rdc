//! Drives `.github/scripts/release-plan.sh` with synthetic `git diff` / `git log`
//! output. The script is pure -- two input files in, key=value lines out -- so
//! the weekly release decision is testable without a repository, a tag, or a
//! network.
#![cfg(unix)]

use std::fs;
use std::process::Command;

use tempfile::TempDir;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.github/scripts/release-plan.sh");

/// Runs the script over the given changed-paths and commit-log text.
/// Returns `(exited zero, stdout)`.
fn plan(version: &str, paths: &str, log: &str) -> (bool, String) {
    let dir = TempDir::new().unwrap();
    let paths_file = dir.path().join("paths");
    let log_file = dir.path().join("log");
    fs::write(&paths_file, paths).unwrap();
    fs::write(&log_file, log).unwrap();
    let out = Command::new("sh")
        .arg(SCRIPT)
        .arg(version)
        .arg(&paths_file)
        .arg(&log_file)
        .output()
        .expect("sh is available");
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
    assert_eq!(out, "release=false\n");
}

#[test]
fn an_empty_range_releases_nothing() {
    let (ok, out) = plan("0.7.0", "", "");
    assert!(ok);
    assert_eq!(out, "release=false\n");
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
    assert_eq!(out, "release=false\n");
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
