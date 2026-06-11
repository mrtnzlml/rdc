// The cwd_lock() guard is intentionally held across calls that mutate the
// process-wide current directory, serializing tests in this binary.
#![allow(clippy::await_holding_lock)]

//! Integration tests for the pure-local `rdc migrate <src> <tgt>` command.
//!
//! `migrate` makes ZERO remote calls — these tests never start a mock server.
//! They build a two-env project on disk, write a source snapshot carrying
//! `rdc://` portable refs, run the transform, and assert the target snapshot's
//! files land at remapped paths with remapped refs + applied overlays.

use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

fn cwd_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Bootstrap a two-env project (`test` + `prod`) via the `init` subcommand,
/// the same shape every integration test in this crate uses.
fn init_two_env_project() -> TempDir {
    let project = TempDir::new().unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--env",
            "test=https://test.example/api/v1:1",
            "--env",
            "prod=https://prod.example/api/v1:2",
        ])
        .assert()
        .success();
    project
}

fn write(path: &std::path::Path, body: &serde_json::Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec_pretty(body).unwrap()).unwrap();
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// End-to-end migrate of a src snapshot with a renamed queue + workspace and
/// an identity hook: target files land at remapped paths, `rdc://` refs are
/// rewritten for the renamed pair, identity refs survive, and the tgt overlay
/// is applied. No network is touched (no client is ever constructed).
#[test]
fn migrate_copies_and_remaps_snapshot_offline() {
    let project = init_two_env_project();
    let root = project.path();

    // --- source snapshot (env `test`) ---
    let test_root = root.join("envs/test");
    // workspace
    write(
        &test_root.join("workspaces/main/workspace.json"),
        &serde_json::json!({ "name": "Main" }),
    );
    // queue referencing workspace + schema + an identity hook
    write(
        &test_root.join("workspaces/main/queues/invoices/queue.json"),
        &serde_json::json!({
            "name": "Invoices",
            "workspace": "rdc://workspaces/main",
            "schema": "rdc://schemas/invoices",
            "hooks": ["rdc://hooks/extractor"],
        }),
    );
    write(
        &test_root.join("workspaces/main/queues/invoices/schema.json"),
        &serde_json::json!({ "name": "Invoices schema", "content": [] }),
    );
    // identity hook + its .py sidecar
    write(
        &test_root.join("hooks/extractor.json"),
        &serde_json::json!({ "name": "Extractor", "type": "function" }),
    );
    std::fs::write(
        test_root.join("hooks/extractor.py"),
        b"def f(p):\n    return {}\n",
    )
    .unwrap();

    // --- mapping: rename main->main-prod, invoices->invoices-prod ---
    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        map_dir.join("test-to-prod.toml"),
        r#"
version = 1

[workspaces]
"main" = "main-prod"

[queues]
"invoices" = "invoices-prod"

[schemas]
"invoices" = "invoices-prod"

[hooks]
"extractor" = "extractor"
"#,
    )
    .unwrap();

    // --- tgt overlay keyed by the TARGET slug ---
    std::fs::write(
        root.join("envs/prod/overlay.toml"),
        "version = 1\n\n[hooks.extractor]\n\"name\" = \"Extractor (PROD)\"\n",
    )
    .unwrap();

    // Run migrate from the project root.
    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![]);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed offline");

    let prod_root = root.join("envs/prod");

    // Queue landed at the remapped path with remapped refs + identity ref intact.
    let q = read_json(&prod_root.join("workspaces/main-prod/queues/invoices-prod/queue.json"));
    assert_eq!(q["workspace"], "rdc://workspaces/main-prod");
    assert_eq!(q["schema"], "rdc://schemas/invoices-prod");
    assert_eq!(
        q["hooks"][0], "rdc://hooks/extractor",
        "identity ref survives"
    );

    // Workspace + schema files exist at remapped locations.
    assert!(
        prod_root
            .join("workspaces/main-prod/workspace.json")
            .exists()
    );
    assert!(
        prod_root
            .join("workspaces/main-prod/queues/invoices-prod/schema.json")
            .exists()
    );

    // Identity hook: overlay applied, .py copied verbatim.
    let hook = read_json(&prod_root.join("hooks/extractor.json"));
    assert_eq!(hook["name"], "Extractor (PROD)", "tgt overlay applied");
    assert_eq!(
        std::fs::read(prod_root.join("hooks/extractor.py")).unwrap(),
        b"def f(p):\n    return {}\n"
    );

    // Mapping was persisted (auto-match would also fill same-slug pairs).
    assert!(root.join(".rdc/map/test-to-prod.toml").exists());
}

/// `--dry-run` prints the plan and writes nothing to the target snapshot.
#[test]
fn migrate_dry_run_writes_nothing() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/hooks/extractor.json"),
        &serde_json::json!({ "name": "Extractor" }),
    );

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, true, vec![]);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("dry-run migrate should succeed");

    assert!(
        !root.join("envs/prod/hooks/extractor.json").exists(),
        "dry-run must not write target files"
    );
}

/// `validate_mapping_sources` aborts before any write when the mapping names a
/// source object that doesn't exist on disk.
#[test]
fn migrate_errors_on_stale_mapping_source() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/hooks/extractor.json"),
        &serde_json::json!({ "name": "Extractor" }),
    );

    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        map_dir.join("test-to-prod.toml"),
        "version = 1\n\n[hooks]\n\"ghost\" = \"ghost-prod\"\n",
    )
    .unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![]);
    std::env::set_current_dir(&prev).unwrap();

    let err = result.expect_err("stale mapping source must abort migrate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ghost"),
        "error must name the missing source: {msg}"
    );
    assert!(
        !root.join("envs/prod/hooks/extractor.json").exists(),
        "no target files written when validation fails"
    );
}

/// `--only <selector>` restricts the migration to matching objects; objects
/// outside the selection are not written to the target snapshot. This
/// preserves the coverage of the former `deploy_only_*` tests now that the
/// `--only` filter lives on `migrate` (it reuses deploy's pure-fs selection
/// machinery — `deploy::selection::resolve`).
#[test]
fn migrate_only_restricts_to_selected_object() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");

    // Two hooks on disk; only one is selected.
    write(
        &test_root.join("hooks/keeper.json"),
        &serde_json::json!({ "name": "Keeper", "type": "function" }),
    );
    write(
        &test_root.join("hooks/skipped.json"),
        &serde_json::json!({ "name": "Skipped", "type": "function" }),
    );

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec!["hooks/keeper".into()]);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate --only should succeed");

    let prod_root = root.join("envs/prod");
    assert!(
        prod_root.join("hooks/keeper.json").exists(),
        "selected hook must be migrated"
    );
    assert!(
        !prod_root.join("hooks/skipped.json").exists(),
        "unselected hook must NOT be migrated under --only"
    );
}

#[test]
fn migrate_only_includes_sidecars_of_selected_objects() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");

    // Hook with a code sidecar — selecting the hook must carry the .py along.
    write(
        &test_root.join("hooks/keeper.json"),
        &serde_json::json!({ "name": "Keeper", "type": "function" }),
    );
    std::fs::write(test_root.join("hooks/keeper.py"), b"def k(): pass\n").unwrap();
    // Unselected hook + sidecar must both stay behind.
    write(
        &test_root.join("hooks/skipped.json"),
        &serde_json::json!({ "name": "Skipped", "type": "function" }),
    );
    std::fs::write(test_root.join("hooks/skipped.py"), b"def s(): pass\n").unwrap();

    // Rule with a trigger-condition sidecar.
    write(
        &test_root.join("rules/validation.json"),
        &serde_json::json!({ "name": "Validation" }),
    );
    std::fs::write(test_root.join("rules/validation.py"), b"x > 0\n").unwrap();

    // Schema with a formula sidecar nested under its queue.
    let qdir = test_root.join("workspaces/main/queues/cost-invoices");
    write(
        &qdir.join("schema.json"),
        &serde_json::json!({ "name": "Cost invoices schema", "content": [] }),
    );
    std::fs::create_dir_all(qdir.join("formulas")).unwrap();
    std::fs::write(
        qdir.join("formulas/total_amount.py"),
        b"field.total_amount\n",
    )
    .unwrap();
    write(
        &test_root.join("workspaces/main/workspace.json"),
        &serde_json::json!({ "name": "Main" }),
    );

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run(
        "test",
        "prod",
        false,
        false,
        vec![
            "hooks/keeper".into(),
            "rules/validation".into(),
            "schemas/cost-invoices".into(),
        ],
    );
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate --only should succeed");

    let prod_root = root.join("envs/prod");
    assert!(
        prod_root.join("hooks/keeper.py").exists(),
        "selected hook's .py sidecar must be migrated"
    );
    assert!(
        !prod_root.join("hooks/skipped.py").exists(),
        "unselected hook's sidecar must NOT be migrated"
    );
    assert!(
        prod_root.join("rules/validation.py").exists(),
        "selected rule's .py sidecar must be migrated"
    );
    assert!(
        prod_root
            .join("workspaces/main/queues/cost-invoices/formulas/total_amount.py")
            .exists(),
        "selected schema's formula sidecars must be migrated"
    );
}

/// An `--only` selector that matches nothing aborts loudly rather than
/// silently producing an empty migration (preserves the coverage of the
/// former `deploy_only_with_unknown_selector_errors`).
#[test]
fn migrate_only_unknown_selector_errors() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/hooks/keeper.json"),
        &serde_json::json!({ "name": "Keeper", "type": "function" }),
    );

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run(
        "test",
        "prod",
        false,
        false,
        vec!["hooks/does-not-exist".into()],
    );
    std::env::set_current_dir(&prev).unwrap();

    let err = result.expect_err("an --only selector matching nothing must abort");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("matched 0 objects"),
        "error must explain the selector matched nothing: {msg}"
    );
}

/// A4 — the `rdc migrate <src> <tgt>` binary subcommand transforms the
/// snapshot and exits 0, with no network server in sight.
#[test]
fn migrate_binary_subcommand_transforms_snapshot() {
    let project = init_two_env_project();
    let root = project.path();

    write(
        &root.join("envs/test/hooks/extractor.json"),
        &serde_json::json!({ "name": "Extractor", "type": "function" }),
    );

    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "test", "prod"])
        .assert()
        .success();

    let migrated = read_json(&root.join("envs/prod/hooks/extractor.json"));
    assert_eq!(migrated["name"], "Extractor");
    assert_eq!(migrated["type"], "function");
}

/// A4 — `--dry-run` via the binary writes nothing.
#[test]
fn migrate_binary_dry_run_writes_nothing() {
    let project = init_two_env_project();
    let root = project.path();

    write(
        &root.join("envs/test/hooks/extractor.json"),
        &serde_json::json!({ "name": "Extractor" }),
    );

    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "test", "prod", "--dry-run"])
        .assert()
        .success();

    assert!(
        !root.join("envs/prod/hooks/extractor.json").exists(),
        "binary --dry-run must not write target files"
    );
}

/// Migrate MUST preserve the TARGET object's env-specific identity (id, url-host
/// fields, created_by/modified_by, organization) and only carry over the
/// source's deployable CONTENT. Regression for the bug where migrate copied the
/// source env's `id` and `created_by`/`modified_by`/`organization` into the
/// target, making every object claim the wrong (source) identity.
#[test]
fn migrate_preserves_target_identity_for_matched_object() {
    let project = init_two_env_project(); // envs: test (host test.example), prod (prod.example)
    let root = project.path();

    // SOURCE (test) hook: src identity + src content.
    write(
        &root.join("envs/test/hooks/extractor.json"),
        &serde_json::json!({
            "id": 100,
            "url": "rdc://hooks/extractor",
            "name": "Extractor NEW NAME",
            "type": "function",
            "events": ["annotation_content.started"],
            "created_by": "https://test.example/api/v1/users/11",
            "modified_by": "https://test.example/api/v1/users/11",
            "organization": "https://test.example/api/v1/organizations/1",
            "queues": [],
        }),
    );
    // TARGET (prod) hook ALREADY EXISTS with its own identity + old content.
    write(
        &root.join("envs/prod/hooks/extractor.json"),
        &serde_json::json!({
            "id": 999,
            "url": "rdc://hooks/extractor",
            "name": "Extractor OLD NAME",
            "type": "function",
            "events": ["annotation_content.initialize"],
            "created_by": "https://prod.example/api/v1/users/77",
            "modified_by": "https://prod.example/api/v1/users/77",
            "organization": "https://prod.example/api/v1/organizations/2",
            "queues": [],
        }),
    );
    // identity mapping
    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        map_dir.join("test-to-prod.toml"),
        "version = 1\n\n[hooks]\n\"extractor\" = \"extractor\"\n",
    )
    .unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![]);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let h = read_json(&root.join("envs/prod/hooks/extractor.json"));
    // Identity preserved from TARGET:
    assert_eq!(h["id"], 999, "target id must be preserved, not src's 100");
    assert_eq!(
        h["created_by"], "https://prod.example/api/v1/users/77",
        "target created_by must be preserved (prod host), not src's"
    );
    assert_eq!(
        h["modified_by"], "https://prod.example/api/v1/users/77",
        "target modified_by must be preserved"
    );
    assert_eq!(
        h["organization"], "https://prod.example/api/v1/organizations/2",
        "target organization must be preserved"
    );
    // Content migrated from SOURCE:
    assert_eq!(h["name"], "Extractor NEW NAME", "src content (name) migrated");
    assert_eq!(h["events"][0], "annotation_content.started", "src content (events) migrated");
}

/// For an object that does NOT exist in the target (new), migrate must strip the
/// source's server-assigned identity (so `rdc sync` POSTs a clean create), not
/// carry the source env's id/created_by across.
#[test]
fn migrate_strips_identity_for_new_object() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/hooks/brand-new.json"),
        &serde_json::json!({
            "id": 100,
            "url": "rdc://hooks/brand-new",
            "name": "Brand New",
            "type": "function",
            "created_by": "https://test.example/api/v1/users/11",
            "organization": "https://test.example/api/v1/organizations/1",
            "queues": [],
        }),
    );
    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        map_dir.join("test-to-prod.toml"),
        "version = 1\n\n[hooks]\n\"brand-new\" = \"brand-new\"\n",
    )
    .unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    rdc::cli::migrate::run("test", "prod", false, false, vec![]).expect("migrate ok");
    std::env::set_current_dir(&prev).unwrap();

    let h = read_json(&root.join("envs/prod/hooks/brand-new.json"));
    assert!(h.get("id").is_none(), "new object must not carry src id; got {:?}", h.get("id"));
    assert!(h.get("url").is_none(), "new object must not carry src url");
    assert!(h.get("created_by").is_none(), "new object must not carry src created_by");
    // organization is set to the TARGET org (prod), never the source's.
    assert_eq!(
        h["organization"], "https://prod.example/api/v1/organizations/2",
        "new object organization must be the target org, not src's"
    );
    assert_eq!(h["name"], "Brand New", "content preserved for the create");
}
