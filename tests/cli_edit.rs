//! End-to-end `rdc edit env rename` on a project scaffolded by `rdc init`.

use assert_cmd::Command;
use tempfile::TempDir;

fn scaffolded() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("rdc.toml"),
        "[envs.dev]\napi_base = \"https://dev.example.test/api/v1\"\norg_id = 1\n\n\
         [envs.prod]\napi_base = \"https://prod.example.test/api/v1\"\norg_id = 2\n",
    )
    .unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(dir.path()).args(["init", "--force"]).assert().success();
    assert!(dir.path().join(".gitlab-ci.yml").exists(), "init should scaffold the pipeline");
    std::fs::create_dir_all(dir.path().join("envs/dev")).unwrap();
    std::fs::write(dir.path().join("envs/dev/organization.json"), "{}").unwrap();
    dir
}

fn rdc(dir: &TempDir, args: &[&str]) -> (bool, String) {
    let out = Command::cargo_bin("rdc").unwrap().current_dir(dir.path()).args(args).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

#[test]
fn renames_the_env_and_reports_it() {
    let dir = scaffolded();
    let (ok, out) = rdc(&dir, &["edit", "env", "rename", "dev", "sandbox"]);
    assert!(ok, "{out}");
    assert!(out.contains("renamed env dev -> sandbox"), "{out}");
    assert!(out.contains("moved    envs/dev -> envs/sandbox"), "{out}");
    assert!(out.contains("rewrote  rdc.toml"), "{out}");
    assert!(out.contains("still to do in GitLab:"), "{out}");
    assert!(out.contains("RDC_TOKEN_DEV to RDC_TOKEN_SANDBOX"), "{out}");
    assert!(dir.path().join("envs/sandbox/organization.json").exists());
    let toml = std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap();
    assert!(toml.contains("[envs.sandbox]") && !toml.contains("[envs.dev]"), "{toml}");

    // init afterwards has nothing left to fix: no new draft for `sandbox`.
    // `--force` is init's no-`--env` form; a markered pipeline is still
    // only spliced under it.
    let ci_before = std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap();
    let (ok, out) = rdc(&dir, &["init", "--force"]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap(), ci_before);
}

#[test]
fn dry_run_writes_nothing() {
    let dir = scaffolded();
    let toml_before = std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap();
    let (ok, out) = rdc(&dir, &["edit", "env", "rename", "dev", "sandbox", "--dry-run"]);
    assert!(ok, "{out}");
    assert!(out.contains("would rename env dev -> sandbox"), "{out}");
    assert!(dir.path().join("envs/dev/organization.json").exists());
    assert_eq!(std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap(), toml_before);
}

#[test]
fn an_unknown_env_fails() {
    let dir = scaffolded();
    let (ok, out) = rdc(&dir, &["edit", "env", "rename", "nope", "sandbox"]);
    assert!(!ok);
    assert!(out.contains("This project has no \"nope\" environment."), "{out}");
}
