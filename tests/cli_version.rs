use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn version_flag_prints_version() {
    Command::cargo_bin("rdc")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("rdc 0.0.1"));
}
