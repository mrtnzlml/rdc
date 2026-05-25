use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(format!("testdata/fixtures/{name}")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

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
    // Advisory OS lock files (`.rdc/state/<env>.lock`) are per-machine
    // and intentionally empty; exclude them from the committable
    // snapshot. The committable `<env>.lock.json` lockfile is NOT
    // matched by `*.lock` (it ends in `.json`).
    assert!(gitignore.contains("/.rdc/state/*.lock"));

    // Generated files under .rdc/ (lockfile, mapping) are marked so
    // GitHub collapses their diffs and excludes them from language stats.
    let gitattributes = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
    assert!(gitattributes.contains(".rdc/** linguist-generated=true"));

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
fn init_preserves_existing_gitattributes_and_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let user_attrs = "*.png binary\nMakefile text eol=lf\n";
    std::fs::write(dir.path().join(".gitattributes"), user_attrs).unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success();

    let after = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
    assert!(after.contains("*.png binary"));
    assert!(after.contains("Makefile text eol=lf"));
    assert!(after.contains(".rdc/** linguist-generated=true"));

    // Re-running init on the same project must not duplicate the rule.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "prod=https://example.rossum.app/api/v1:285705",
        ])
        .assert()
        .success();

    let after_second = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
    assert_eq!(
        after_second.matches(".rdc/** linguist-generated=true").count(),
        1,
        "rule should be written exactly once across init runs"
    );
}

#[test]
fn init_appends_missing_gitignore_lines_without_duplicating() {
    // Pre-existing .gitignore already has /secrets and /.rdc/cache
    // (from an older rdc init), plus a user-authored rule. The newer
    // template adds /.rdc/state/*.lock — that line should be appended,
    // the old lines should NOT be duplicated, and the user-authored
    // rule should survive.
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join(".gitignore"),
        "/secrets\n/.rdc/cache\nmy-local-notes/\n",
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success();

    let after = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(after.contains("my-local-notes/"), "user rule must survive");
    assert!(after.contains("/secrets"));
    assert!(after.contains("/.rdc/cache"));
    assert!(after.contains("/.rdc/state/*.lock"));
    // No duplication of pre-existing patterns.
    assert_eq!(after.matches("/secrets").count(), 1);
    assert_eq!(after.matches("/.rdc/cache").count(), 1);

    // Re-running init is fully idempotent — no further changes.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "prod=https://example.rossum.app/api/v1:285705",
        ])
        .assert()
        .success();
    let after_second = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert_eq!(after, after_second, "second init must not touch .gitignore");
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

/// Fresh init must drop a README.md with the run-the-project commands —
/// `rdc sync` for every env defined in `rdc.toml` and pointers to the
/// other doc surfaces. Single-env projects have nothing to deploy, so
/// the Deploy section is conditionally omitted.
#[test]
fn init_creates_readme_with_sync_commands_and_skips_deploy_for_single_env() {
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

    let readme_path = dir.path().join("README.md");
    assert!(readme_path.exists(), "README.md should be created on init");
    let body = std::fs::read_to_string(&readme_path).unwrap();
    assert!(body.contains("## Sync each environment"), "missing sync section: {body}");
    assert!(body.contains("rdc sync dev"), "missing dev sync command: {body}");
    assert!(
        !body.contains("## Deploy"),
        "single-env project should not have a Deploy section: {body}"
    );
    // Cross-references to the other doc surfaces are part of why we
    // generate this file in the first place.
    assert!(body.contains("`CLAUDE.md`"), "missing CLAUDE.md link: {body}");
    assert!(body.contains("_index.md"), "missing _index.md link: {body}");
}

/// Two-env project includes a Deploy section with a concrete example
/// using the alphabetically-first pair (BTreeMap order, deterministic).
#[test]
fn init_readme_includes_deploy_for_multiple_envs() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
            "--env", "prod=https://example.rossum.app/api/v1:285705",
        ])
        .assert()
        .success();

    let body = std::fs::read_to_string(dir.path().join("README.md")).unwrap();
    // Both envs listed in the sync block.
    assert!(body.contains("rdc sync dev"));
    assert!(body.contains("rdc sync prod"));
    // Deploy section appears for multi-env projects.
    assert!(body.contains("## Deploy"), "deploy section missing: {body}");
    // The first two alphabetically: dev → prod.
    assert!(
        body.contains("rdc deploy dev prod"),
        "expected deploy example with dev→prod: {body}"
    );
    assert!(body.contains("--dry-run"), "deploy --dry-run guidance missing: {body}");
}

/// User has authored their own README before running init — we must
/// not touch it (same contract as CLAUDE.md).
#[test]
fn init_does_not_clobber_existing_readme() {
    let dir = TempDir::new().unwrap();
    let user_content = "# My project\n\nCustom notes — keep these.\n";
    std::fs::write(dir.path().join("README.md"), user_content).unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success();

    let after = std::fs::read_to_string(dir.path().join("README.md")).unwrap();
    assert_eq!(after, user_content, "init must not overwrite a pre-existing README.md");
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

/// `RDC_TOKEN_<UPPER>` set at init time is picked up automatically:
/// the token is validated against the Rossum API, the secrets file
/// is written with mode 0600, and the per-env "rdc auth" line is
/// dropped from the next-steps output. The user gets a one-step setup.
#[tokio::test]
async fn init_with_env_var_runs_auth_inline() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .and(header("Authorization", "token TOKEN_FROM_ENV"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .env("RDC_TOKEN_DEV", "TOKEN_FROM_ENV")
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success()
        // The auth phase prints its own validation banner to stderr.
        .stderr(predicate::str::contains("validated against org"))
        // Next-steps drops the per-env auth line when auth succeeded.
        .stdout(predicate::str::contains("Next steps:"))
        .stdout(predicate::str::contains("rdc sync dev"))
        .stdout(predicate::str::contains("rdc auth dev").not());

    let secrets_path = project.path().join("secrets/dev.secrets.json");
    assert!(
        secrets_path.exists(),
        "auth-on-init must write the secrets file when the token validates"
    );
    let body = std::fs::read_to_string(&secrets_path).unwrap();
    assert!(body.contains("TOKEN_FROM_ENV"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&secrets_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "secrets file should be mode 0600");
    }

    // The sync-on-init prompt is TTY-gated; assert_cmd runs the child
    // non-TTY so no implicit sync should have happened. Lockfile absence
    // proves it — `rdc sync` would have created `.rdc/state/dev.lock.json`
    // as part of its baseline write.
    assert!(
        !project.path().join(".rdc/state/dev.lock.json").exists(),
        "init must NOT auto-sync in non-TTY mode (sync prompt is TTY-gated)"
    );
}

/// A bad token coming from `RDC_TOKEN_<UPPER>` must not abort init: the
/// project files are still written, a clear warning is surfaced, no
/// secrets file is written, and the next-steps output keeps the per-env
/// auth line so the user can recover.
#[tokio::test]
async fn init_with_invalid_env_var_token_warns_and_continues() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "detail": "Invalid token.",
            "code": "authentication_failed",
        })))
        .mount(&server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .env("RDC_TOKEN_DEV", "BAD_TOKEN")
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success()
        .stderr(predicate::str::contains("failed validation"))
        .stderr(predicate::str::contains("re-run `rdc auth dev`"))
        // Project files are written normally.
        .stdout(predicate::str::contains("Initialized"))
        // Next-steps still asks the user to set up auth.
        .stdout(predicate::str::contains("rdc auth dev"));

    assert!(
        project.path().join("rdc.toml").exists(),
        "init must still write rdc.toml when auth validation fails"
    );
    assert!(
        !project.path().join("secrets/dev.secrets.json").exists(),
        "no secrets file written when token is rejected"
    );
}

/// Non-TTY init with no `RDC_TOKEN_<UPPER>` set keeps the original
/// behavior: project files written, no API calls attempted, and the
/// next-steps message walks the user through `rdc auth`.
#[test]
fn init_without_env_var_in_ci_skips_auth() {
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .env_remove("RDC_TOKEN_DEV")
        .args(["init", "--env", "dev=https://example.rossum.app/api/v1:285704"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rdc auth dev"))
        .stderr(predicate::str::contains("Validated against org").not());

    assert!(
        !project.path().join("secrets/dev.secrets.json").exists(),
        "without a token source, no secrets file should be written"
    );
}

/// Real env names contain hyphens (`dev-ap`, `prod-eu`, …). The
/// `RDC_TOKEN_<UPPER>` convention must normalize non-alphanumerics to
/// `_` so the shell can actually export the variable, and the
/// next-steps hint must quote the normalized form.
#[tokio::test]
async fn init_with_hyphenated_env_uses_normalized_env_var() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .and(header("Authorization", "token TOKEN_FROM_ENV"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        // The motivating real-world example.
        .env("RDC_TOKEN_DEV_AP", "TOKEN_FROM_ENV")
        .args(["init", "--env", &format!("dev-ap={}/api/v1:1", server.uri())])
        .assert()
        .success()
        .stderr(predicate::str::contains("validated against org"));

    assert!(
        project.path().join("secrets/dev-ap.secrets.json").exists(),
        "secrets file path keeps the original env name (with hyphen)"
    );
}

/// If `RDC_TOKEN_DEV_AP` isn't set, the printed "Next steps" must
/// suggest the normalized var name — not the invalid hyphenated form
/// (which is what previous versions emitted).
#[test]
fn init_next_steps_quotes_normalized_env_var_for_hyphenated_env() {
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .env_remove("RDC_TOKEN_DEV_AP")
        .args(["init", "--env", "dev-ap=https://x.rossum.app/api/v1:1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("RDC_TOKEN_DEV_AP"))
        .stdout(predicate::str::contains("RDC_TOKEN_DEV-AP").not());
}

/// Two env names that normalize to the same shell variable can't
/// coexist — one would steal the other's token. Init refuses the
/// second add with a clear message naming both envs and the variable.
#[test]
fn init_refuses_env_name_that_collides_with_existing_env_var() {
    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", "dev-ap=https://x.rossum.app/api/v1:1"])
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", "dev_ap=https://x.rossum.app/api/v1:2"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RDC_TOKEN_DEV_AP"))
        .stderr(predicate::str::contains("dev-ap"))
        .stderr(predicate::str::contains("dev_ap"));
}
