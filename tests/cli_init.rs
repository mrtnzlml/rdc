use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn init_creates_expected_files() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--name", "demo",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized"));

    assert!(dir.path().join("rdc.toml").exists());
    assert!(dir.path().join(".gitignore").exists());
    assert!(dir.path().join("envs/dev").is_dir());
    assert!(dir.path().join("secrets").is_dir());

    let cfg = std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap();
    assert!(cfg.contains("name = \"demo\""));
    assert!(cfg.contains("[envs.dev]"));
    assert!(cfg.contains("api_base = \"https://example.rossum.app/api/v1\""));

    let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains("/secrets"));
    assert!(gitignore.contains("/.rdc/cache"));
}

#[test]
fn init_refuses_to_clobber_existing_project() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("rdc.toml"), "stub").unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--name", "demo",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already initialized"));
}

/// In CI / piped contexts, stdin is not a TTY. `rdc init` with no flags
/// should fail with a useful usage hint rather than block on stdin.
#[test]
fn init_without_flags_in_ci_errors_with_usage_hint() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init"])
        // assert_cmd's runner inherits the parent's stdin; in `cargo test`
        // that's the test runner's stdin, which is not a TTY. So
        // `is_terminal()` returns false and we should get the usage error.
        .assert()
        .failure()
        .stderr(predicate::str::contains("--name and at least one --env are required"));
    assert!(!dir.path().join("rdc.toml").exists(), "no project should be scaffolded on error");
}

/// Init should print clear next-step hints (token + pull) so users know
/// what to do after scaffolding.
#[test]
fn init_prints_next_steps() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--name", "demo",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Next steps:"))
        .stdout(predicate::str::contains("rdc auth dev"))
        .stdout(predicate::str::contains("rdc pull dev"));
}
