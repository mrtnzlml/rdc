//! `rdc upgrade` end to end, against a mock GitHub.
//!
//! Each test copies the real rdc binary into a temp directory and lets it
//! replace itself. `RDC_UPGRADE_URL_BASE` points both the release API and the
//! asset download at one wiremock server. The "release" is a tarball holding
//! a shell script that prints a version, which is all the pre-flight
//! `--version` check asks of it — hence Unix only.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A release tarball shaped like release.yaml's: `rdc` at the root.
fn release_tarball(version: &str) -> Vec<u8> {
    let script = format!("#!/bin/sh\necho \"rdc {version}\"\n");
    let mut header = tar::Header::new_gnu();
    header.set_size(script.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(gz);
    archive.append_data(&mut header, "rdc", script.as_bytes()).unwrap();
    archive.into_inner().unwrap().finish().unwrap()
}

/// A copy of the rdc under test, in a directory it may overwrite.
fn installed_rdc(dir: &Path) -> PathBuf {
    let bin = dir.join("rdc");
    std::fs::copy(assert_cmd::cargo::cargo_bin("rdc"), &bin).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn run(bin: &Path, home: &Path, server: &MockServer, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .env("RDC_UPGRADE_URL_BASE", server.uri())
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env_remove("CARGO_HOME")
        .output()
        .unwrap()
}

fn version_of(bin: &Path) -> String {
    let out = Command::new(bin).arg("--version").output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

async fn latest_release_is(server: &MockServer, tag: &str) {
    Mock::given(method("GET"))
        .and(path("/repos/mrtnzlml/rdc/releases/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "tag_name": tag })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn upgrade_installs_the_latest_release_and_keeps_a_backup() {
    let server = MockServer::start().await;
    latest_release_is(&server, "v99.0.0").await;
    // Releases since v0.7.0 carry the version in the asset name.
    Mock::given(method("GET"))
        .and(path_regex(r"^/mrtnzlml/rdc/releases/download/v99\.0\.0/rdc-99\.0\.0-[a-z0-9_]+-[a-z-]+\.tar\.gz$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(release_tarball("99.0.0")))
        .expect(1)
        .mount(&server)
        .await;

    let home = TempDir::new().unwrap();
    let bin = installed_rdc(home.path());
    let out = run(&bin, home.path(), &server, &["upgrade"]);

    assert!(out.status.success(), "upgrade failed: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(version_of(&bin), "rdc 99.0.0");
    assert_eq!(
        version_of(&home.path().join("rdc.bak")),
        format!("rdc {}", env!("CARGO_PKG_VERSION")),
        "rdc.bak must be the binary that was replaced"
    );
}

#[tokio::test]
async fn upgrade_to_a_release_before_v0_7_0_uses_the_unversioned_asset() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/mrtnzlml/rdc/releases/download/v0\.6\.0/rdc-[a-z0-9_]+-[a-z-]+\.tar\.gz$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(release_tarball("0.6.0")))
        .expect(1)
        .mount(&server)
        .await;

    let home = TempDir::new().unwrap();
    let bin = installed_rdc(home.path());
    let out = run(&bin, home.path(), &server, &["upgrade", "--version", "0.6.0"]);

    assert!(out.status.success(), "upgrade failed: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(version_of(&bin), "rdc 0.6.0");
}

#[tokio::test]
async fn upgrade_leaves_the_binary_alone_when_the_asset_is_missing() {
    let server = MockServer::start().await;
    latest_release_is(&server, "v99.0.0").await;
    // No asset mock: the download answers 404.

    let home = TempDir::new().unwrap();
    let bin = installed_rdc(home.path());
    let before = std::fs::read(&bin).unwrap();
    let out = run(&bin, home.path(), &server, &["upgrade"]);

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("404"));
    assert_eq!(std::fs::read(&bin).unwrap(), before, "the installed binary must not change");
    assert!(!home.path().join("rdc.bak").exists());
}
