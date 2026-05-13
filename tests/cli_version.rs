use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn version_flag_prints_version() {
    // Pull the current version from cargo metadata so version bumps in
    // `Cargo.toml` don't silently break this test. `CARGO_PKG_VERSION`
    // is set by Cargo for both the crate under test and the test
    // binary (same version).
    let expected = format!("rdc {}", env!("CARGO_PKG_VERSION"));
    Command::cargo_bin("rdc")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(expected));
}
