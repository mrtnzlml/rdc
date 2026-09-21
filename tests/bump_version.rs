//! Drives `.github/scripts/bump-version.sh` over a synthetic four-file tree.
//! The script refuses any edit that is not exactly one line, which is the guard
//! that keeps an unattended release from quietly rewriting a lockfile.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.github/scripts/bump-version.sh");

const CARGO_TOML: &str = r#"[package]
name = "rdc"
version = "0.7.0"
edition.workspace = true

[dependencies]
anyhow = "1"

[workspace.package]
edition = "2024"
"#;

const ROOT_LOCK: &str = r#"version = 4

[[package]]
name = "anyhow"
version = "1.0.100"

[[package]]
name = "rdc"
version = "0.7.0"
dependencies = [
 "anyhow",
]
"#;

const DESKTOP_LOCK: &str = r#"version = 4

[[package]]
name = "rdc"
version = "0.7.0"
dependencies = [
 "anyhow",
]

[[package]]
name = "rdc_bridge"
version = "0.1.0"
"#;

/// The scaffolded pipeline names no version at all, so a bump must leave it
/// exactly as it is.
const TEMPLATE: &str = r#"variables:
  RDC_RELEASE: "latest"
  RDC_REPO: "mrtnzlml/rdc"
"#;

fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("Cargo.toml"), CARGO_TOML).unwrap();
    fs::write(dir.path().join("Cargo.lock"), ROOT_LOCK).unwrap();
    fs::create_dir_all(dir.path().join("desktop/rust")).unwrap();
    fs::write(dir.path().join("desktop/rust/Cargo.lock"), DESKTOP_LOCK).unwrap();
    fs::create_dir_all(dir.path().join("templates")).unwrap();
    fs::write(dir.path().join("templates/gitlab-ci.yml"), TEMPLATE).unwrap();
    dir
}

fn bump(root: &Path, version: &str) -> Output {
    Command::new("sh")
        .arg(SCRIPT)
        .arg(version)
        .arg(root)
        .output()
        .expect("sh is available")
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn bumps_all_three_files() {
    let t = tree();
    let out = bump(t.path(), "0.8.0");
    assert!(out.status.success(), "{}", stderr(&out));

    assert!(read(t.path(), "Cargo.toml").contains("\nversion = \"0.8.0\"\n"));
    assert!(read(t.path(), "Cargo.lock").contains("name = \"rdc\"\nversion = \"0.8.0\"\n"));
    assert!(
        read(t.path(), "desktop/rust/Cargo.lock").contains("name = \"rdc\"\nversion = \"0.8.0\"\n")
    );
    assert_eq!(read(t.path(), "templates/gitlab-ci.yml"), TEMPLATE, "the template was rewritten");
}

#[test]
fn leaves_every_other_version_line_alone() {
    let t = tree();
    assert!(bump(t.path(), "0.8.0").status.success());

    // A dependency's pin, the lockfile format version, the sibling crate's own
    // version and the historical "pre-0.7" comment must all survive untouched.
    let root_lock = read(t.path(), "Cargo.lock");
    assert!(root_lock.starts_with("version = 4\n"));
    assert!(root_lock.contains("name = \"anyhow\"\nversion = \"1.0.100\""));
    assert!(
        read(t.path(), "desktop/rust/Cargo.lock")
            .contains("name = \"rdc_bridge\"\nversion = \"0.1.0\"")
    );
    assert!(read(t.path(), "Cargo.toml").contains("anyhow = \"1\""));
}

#[test]
fn refuses_a_tree_where_the_crate_version_is_ambiguous() {
    let t = tree();
    // Two `version = "..."` lines at column 0 -- the script cannot know which is
    // the crate's, so it must refuse rather than pick one.
    fs::write(
        t.path().join("Cargo.toml"),
        format!("{CARGO_TOML}\n[some-other-table]\nversion = \"9.9.9\"\n"),
    )
    .unwrap();

    let out = bump(t.path(), "0.8.0");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("expected 1 line matching"), "{}", stderr(&out));
    // Cargo.toml is edited first, so nothing else was written either.
    assert!(read(t.path(), "Cargo.lock").contains("version = \"0.7.0\""));
}

#[test]
fn refuses_a_missing_file() {
    let t = tree();
    fs::remove_file(t.path().join("desktop/rust/Cargo.lock")).unwrap();
    let out = bump(t.path(), "0.8.0");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("missing"), "{}", stderr(&out));
}

#[test]
fn refuses_a_non_semver_version() {
    let t = tree();
    // A leading "v" is the tag's business, not the crate version's.
    let out = bump(t.path(), "v0.8.0");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("is not an x.y.z version"), "{}", stderr(&out));
    assert!(read(t.path(), "Cargo.toml").contains("version = \"0.7.0\""));
}

#[test]
fn refuses_a_tree_already_at_the_target_version() {
    // A no-op edit means the tree was not in the state the caller believed it
    // was, which is worth a red run rather than a silent success.
    let t = tree();
    let out = bump(t.path(), "0.7.0");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("expected a 1-line change, got 0"), "{}", stderr(&out));
}

#[test]
fn leaves_no_temp_files_behind() {
    let t = tree();
    assert!(bump(t.path(), "0.8.0").status.success());
    for rel in [
        "Cargo.toml.tmp",
        "Cargo.lock.tmp",
        "desktop/rust/Cargo.lock.tmp",
        "templates/gitlab-ci.yml.tmp",
    ] {
        assert!(!t.path().join(rel).exists(), "{rel} was left behind");
    }
}

#[test]
fn bumps_the_real_repository_tree_in_a_copy() {
    // Guards the three real files against a rename or a reshuffle: copy them out
    // of the checkout and bump the copy. Never touches the working tree.
    let t = TempDir::new().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::create_dir_all(t.path().join("desktop/rust")).unwrap();
    for rel in ["Cargo.toml", "Cargo.lock", "desktop/rust/Cargo.lock"] {
        fs::copy(repo.join(rel), t.path().join(rel)).unwrap();
    }

    let current = env!("CARGO_PKG_VERSION");
    let out = bump(t.path(), "99.99.99");
    assert!(out.status.success(), "{}", stderr(&out));

    assert!(read(t.path(), "Cargo.toml").contains("\nversion = \"99.99.99\"\n"));
    assert!(read(t.path(), "Cargo.lock").contains("name = \"rdc\"\nversion = \"99.99.99\"\n"));
    assert!(
        read(t.path(), "desktop/rust/Cargo.lock")
            .contains("name = \"rdc\"\nversion = \"99.99.99\"\n")
    );
    // And the old version is gone from the one file that spells it out.
    assert!(
        !read(t.path(), "Cargo.toml").contains(current),
        "Cargo.toml still names {current}"
    );
}
