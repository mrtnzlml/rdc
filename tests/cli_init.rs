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
    // No [project] section any more — the config is just envs.
    assert!(!cfg.contains("[project]"));
    assert!(cfg.contains("[envs.dev]"));
    assert!(cfg.contains("api_base = \"https://example.rossum.app/api/v1\""));

    let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains("/secrets"));
    assert!(gitignore.contains("/.rdc/cache"));

    // CLAUDE.md agent guide is created at project root.
    let claude_md = dir.path().join("CLAUDE.md");
    assert!(claude_md.exists(), "CLAUDE.md should be created on init");
    let body = std::fs::read_to_string(&claude_md).unwrap();
    assert!(body.contains("# Agent guide"));
    assert!(body.contains("`envs/<env>/_index.md`"));
    assert!(body.contains("rdc sync"));
    assert!(body.contains("Conflicts & drift"));
}

#[test]
fn init_does_not_clobber_existing_claude_md() {
    let dir = TempDir::new().unwrap();
    let user_content = "# My own notes\n\nKeep this!\n";
    std::fs::write(dir.path().join("CLAUDE.md"), user_content).unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success();

    let after = std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
    assert_eq!(after, user_content, "init must not overwrite a pre-existing CLAUDE.md");
}

#[test]
fn init_adds_new_env_to_existing_project() {
    let dir = TempDir::new().unwrap();
    // Bootstrap.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success();

    // Re-run init to add a second env. Should succeed.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "prod=https://example.rossum.app/api/v1:285705",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Added env(s)"))
        .stdout(predicate::str::contains("prod"))
        .stdout(predicate::str::contains("rdc auth prod"))
        .stdout(predicate::str::contains("rdc sync prod"));

    // Config now has both envs.
    let cfg = std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap();
    assert!(cfg.contains("[envs.dev]"));
    assert!(cfg.contains("[envs.prod]"));
    assert!(cfg.contains("285705"));

    // Both env dirs scaffolded.
    assert!(dir.path().join("envs/dev").is_dir());
    assert!(dir.path().join("envs/prod").is_dir());
    assert!(dir.path().join("envs/prod/hooks").is_dir());
}

#[test]
fn init_on_existing_project_rejects_duplicate_env() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success();

    // Re-running with the same env name should fail clearly.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "dev=https://different.rossum.app/api/v1:999",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("env 'dev' already exists"));

    // Config preserved — no partial mutation.
    let cfg = std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap();
    assert!(cfg.contains("285704"), "original env config must be preserved");
    assert!(!cfg.contains("999"), "rejected env must not appear");
}

#[test]
fn init_on_existing_project_without_env_in_ci_errors_with_hint() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args(["init"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("extending existing project"))
        .stderr(predicate::str::contains("at least one --env is required"));
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
        .assert()
        .failure()
        .stderr(predicate::str::contains("at least one --env is required"));
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
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Next steps:"))
        .stdout(predicate::str::contains("rdc auth dev"))
        .stdout(predicate::str::contains("rdc sync dev"));
}

/// A pre-existing rdc.toml that still has the legacy `[project]` section
/// must still load — serde ignores unknown fields. Init in extend mode
/// then drops the section when it re-saves the config.
#[test]
fn init_strips_legacy_project_section_on_extend() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("rdc.toml"),
        r#"[project]
name = "legacy"

[envs.dev]
api_base = "https://example.rossum.app/api/v1"
org_id = 285704
"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "prod=https://example.rossum.app/api/v1:285705",
        ])
        .assert()
        .success();

    let cfg = std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap();
    assert!(!cfg.contains("[project]"), "legacy [project] section should be stripped on re-save");
    assert!(cfg.contains("[envs.dev]"), "existing env must be preserved");
    assert!(cfg.contains("[envs.prod]"), "new env must be added");
}
