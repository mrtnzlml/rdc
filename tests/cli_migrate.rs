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

/// Guard returned by [`cwd_lock`]: holds the global mutex AND restores
/// the working directory captured at lock time when dropped — including
/// on panic. Without the restore, a test that panics inside its
/// `set_current_dir` window leaves the process cwd pointing into its
/// (now deleted) tempdir and every later in-process test fails with
/// `NotFound` — one red test used to cascade into dozens.
struct CwdLock {
    _lock: MutexGuard<'static, ()>,
    prev: Option<std::path::PathBuf>,
}

impl Drop for CwdLock {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.take() {
            let _ = std::env::set_current_dir(prev);
        }
    }
}

fn cwd_lock() -> CwdLock {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    CwdLock {
        _lock: lock,
        prev: std::env::current_dir().ok(),
    }
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true);
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

    // The legacy per-pair file is converted into the generic mapping and
    // then deleted — a clean git diff, no more `.rdc/map/*.toml`.
    assert!(
        !root.join(".rdc/map/test-to-prod.toml").exists(),
        "legacy mapping file must be deleted after conversion"
    );
    let mapping_after = std::fs::read_to_string(root.join(".rdc/mapping.toml")).unwrap();
    assert!(
        mapping_after.contains("main-prod"),
        "converted mapping must carry the workspace rename: {mapping_after}"
    );
    assert!(
        mapping_after.contains("invoices-prod"),
        "converted mapping must carry the queue/schema rename: {mapping_after}"
    );
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
    let result = rdc::cli::migrate::run("test", "prod", false, true, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("dry-run migrate should succeed");

    assert!(
        !root.join("envs/prod/hooks/extractor.json").exists(),
        "dry-run must not write target files"
    );
}

/// A mapping entry whose source object doesn't exist on disk (`ghost`) must
/// NOT abort migrate, and must NOT fabricate a target object — there is
/// simply nothing on the source side to migrate for it. There is no more
/// stale-entry pruning: BOTH the dead `ghost` entry and the live `renamer`
/// entry are converted verbatim into `.rdc/mapping.toml` rows (conversion is
/// a pure legacy-file transform, blind to whether the source object still
/// exists), and the legacy per-pair file is deleted once converted.
#[test]
fn migrate_converts_legacy_mapping_unmatched_rename_is_harmless() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/hooks/extractor.json"),
        &serde_json::json!({ "name": "Extractor" }),
    );
    // A live hand-curated rename for a real source object.
    write(
        &root.join("envs/test/hooks/renamer.json"),
        &serde_json::json!({ "name": "Renamer" }),
    );

    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        map_dir.join("test-to-prod.toml"),
        "version = 1\n\n[hooks]\n\"ghost\" = \"ghost-prod\"\n\"renamer\" = \"renamer-prod\"\n",
    )
    .unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();

    // Migrate SUCCEEDS despite the unmatched entry.
    result.expect("an unmatched mapping source must not abort migrate");
    // The real objects were migrated; the dead `ghost` entry produced nothing.
    assert!(
        root.join("envs/prod/hooks/extractor.json").exists(),
        "the identity-mapped source hook must be migrated"
    );
    assert!(
        root.join("envs/prod/hooks/renamer-prod.json").exists(),
        "the renamed source hook must be migrated under its target slug"
    );
    assert!(
        !root.join("envs/prod/hooks/ghost.json").exists(),
        "a mapping entry with no source object must not fabricate a target object"
    );

    // The legacy file is converted then deleted — no more per-pair files.
    assert!(
        !map_dir.join("test-to-prod.toml").exists(),
        "legacy mapping file must be deleted after conversion"
    );
    let mapping_after = std::fs::read_to_string(root.join(".rdc/mapping.toml")).unwrap();
    assert!(
        mapping_after.contains("renamer-prod"),
        "converted mapping must carry the renamer->renamer-prod rename: {mapping_after}"
    );
    assert!(
        mapping_after.contains("ghost-prod"),
        "converted mapping must also carry the dead ghost->ghost-prod rename: {mapping_after}"
    );
}

/// `--dry-run` must not convert the legacy per-pair mapping file into
/// `.rdc/mapping.toml`, nor delete it — the conversion (and its file-system
/// side effects) only happens on a real (writing) migrate.
#[test]
fn migrate_dry_run_does_not_convert_or_delete_legacy_mapping_file() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/hooks/extractor.json"),
        &serde_json::json!({ "name": "Extractor" }),
    );

    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    let map_body = "version = 1\n\n[hooks]\n\"ghost\" = \"ghost-prod\"\n";
    std::fs::write(map_dir.join("test-to-prod.toml"), map_body).unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, true, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("dry-run migrate should succeed");

    let map_after = std::fs::read_to_string(map_dir.join("test-to-prod.toml")).unwrap();
    assert_eq!(
        map_after, map_body,
        "dry-run must leave the legacy mapping file byte-identical"
    );
    assert!(
        !root.join(".rdc/mapping.toml").exists(),
        "dry-run must not write the generic mapping file"
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec!["hooks/keeper".into()], true);
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
        true,
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
        true,
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true);
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

/// Helper: write a minimal src+tgt queue tree (queue.json + schema.json) with an
/// identity mapping, where the schema has one datapoint and the queue has a
/// `default_score_threshold`. `src_th`/`tgt_th` set the per-datapoint threshold;
/// `src_def`/`tgt_def` set the queue default. Returns the project root.
fn setup_threshold_project(
    src_th: f64,
    tgt_th: f64,
    src_def: f64,
    tgt_def: f64,
) -> TempDir {
    let project = init_two_env_project();
    let root = project.path().to_path_buf();
    let schema = |th: f64| {
        serde_json::json!({
            "name": "Invoices schema",
            "content": [{
                "category": "section", "id": "header",
                "children": [
                    { "category": "datapoint", "id": "amount", "type": "number", "score_threshold": th }
                ]
            }]
        })
    };
    let queue = |def: f64| {
        serde_json::json!({
            "name": "Invoices",
            "workspace": "rdc://workspaces/main",
            "schema": "rdc://schemas/invoices",
            "settings": { "default_score_threshold": def }
        })
    };
    for (env, th, def) in [("test", src_th, src_def), ("prod", tgt_th, tgt_def)] {
        let base = root.join(format!("envs/{env}/workspaces/main/queues/invoices"));
        write(&base.join("schema.json"), &schema(th));
        write(&base.join("queue.json"), &queue(def));
        write(
            &root.join(format!("envs/{env}/workspaces/main/workspace.json")),
            &serde_json::json!({ "name": "Main" }),
        );
    }
    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        map_dir.join("test-to-prod.toml"),
        "version = 1\n\n[workspaces]\n\"main\" = \"main\"\n\n[queues]\n\"invoices\" = \"invoices\"\n\n[schemas]\n\"invoices\" = \"invoices\"\n",
    )
    .unwrap();
    project
}

/// By default (no `--migrate-score-thresholds`), migrate ignores the source's
/// thresholds: a matched target keeps its OWN per-datapoint `score_threshold`
/// and queue `default_score_threshold`.
#[test]
fn migrate_ignores_score_thresholds_by_default() {
    let project = setup_threshold_project(0.5, 0.9, 0.5, 0.85);
    let root = project.path();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], false);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let base = root.join("envs/prod/workspaces/main/queues/invoices");
    let schema = read_json(&base.join("schema.json"));
    assert_eq!(
        schema["content"][0]["children"][0]["score_threshold"],
        serde_json::json!(0.9),
        "matched target must keep its own per-datapoint threshold"
    );
    let queue = read_json(&base.join("queue.json"));
    assert_eq!(
        queue["settings"]["default_score_threshold"],
        serde_json::json!(0.85),
        "matched target must keep its own queue default_score_threshold"
    );
}

/// With `--migrate-score-thresholds`, migrate carries the SOURCE's thresholds
/// verbatim (the pre-existing behavior).
#[test]
fn migrate_carries_score_thresholds_with_flag() {
    let project = setup_threshold_project(0.5, 0.9, 0.5, 0.85);
    let root = project.path();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let base = root.join("envs/prod/workspaces/main/queues/invoices");
    let schema = read_json(&base.join("schema.json"));
    assert_eq!(
        schema["content"][0]["children"][0]["score_threshold"],
        serde_json::json!(0.5),
        "opting in must carry the source per-datapoint threshold"
    );
    let queue = read_json(&base.join("queue.json"));
    assert_eq!(
        queue["settings"]["default_score_threshold"],
        serde_json::json!(0.5),
        "opting in must carry the source queue default_score_threshold"
    );
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
    rdc::cli::migrate::run("test", "prod", false, false, vec![], true).expect("migrate ok");
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

#[test]
fn migrate_overlay_shadow_replaces_formula_sidecar() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");

    write(
        &test_root.join("workspaces/main/workspace.json"),
        &serde_json::json!({ "name": "Main" }),
    );
    write(
        &test_root.join("workspaces/main/queues/invoices/queue.json"),
        &serde_json::json!({ "name": "Invoices" }),
    );
    write(
        &test_root.join("workspaces/main/queues/invoices/schema.json"),
        &serde_json::json!({ "name": "Invoices schema", "content": [] }),
    );
    let src_formula = test_root.join("workspaces/main/queues/invoices/formulas/sftp_path.py");
    std::fs::create_dir_all(src_formula.parent().unwrap()).unwrap();
    std::fs::write(&src_formula, b"\"/Test/path\"\n").unwrap();

    // Shadow override for the prod env, mirroring the target tree.
    let shadow =
        root.join("envs/prod/overlay/workspaces/main/queues/invoices/formulas/sftp_path.py");
    std::fs::create_dir_all(shadow.parent().unwrap()).unwrap();
    std::fs::write(&shadow, b"\"/Prod/path\"\n").unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let prod_formula =
        root.join("envs/prod/workspaces/main/queues/invoices/formulas/sftp_path.py");
    assert_eq!(
        std::fs::read_to_string(&prod_formula).unwrap(),
        "\"/Prod/path\"\n",
        "shadow content must replace the source formula"
    );
}

#[test]
fn migrate_overlay_shadow_replaces_hook_and_rule_code_and_leaves_others() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");

    // Two hooks (one shadowed, one not) + a rule (shadowed).
    write(&test_root.join("hooks/extractor.json"), &serde_json::json!({ "name": "Extractor", "type": "function" }));
    std::fs::write(test_root.join("hooks/extractor.py"), b"def f(p):\n    return 'src'\n").unwrap();
    write(&test_root.join("hooks/other.json"), &serde_json::json!({ "name": "Other", "type": "function" }));
    std::fs::write(test_root.join("hooks/other.py"), b"def g(p):\n    return 'keep'\n").unwrap();
    write(&test_root.join("rules/r1.json"), &serde_json::json!({ "name": "R1" }));
    std::fs::write(test_root.join("rules/r1.py"), b"src_condition\n").unwrap();

    // Shadows for the hook + the rule only.
    let ov = root.join("envs/prod/overlay");
    std::fs::create_dir_all(ov.join("hooks")).unwrap();
    std::fs::create_dir_all(ov.join("rules")).unwrap();
    std::fs::write(ov.join("hooks/extractor.py"), b"def f(p):\n    return 'prod'\n").unwrap();
    std::fs::write(ov.join("rules/r1.py"), b"prod_condition\n").unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let prod = root.join("envs/prod");
    assert_eq!(std::fs::read_to_string(prod.join("hooks/extractor.py")).unwrap(), "def f(p):\n    return 'prod'\n");
    assert_eq!(std::fs::read_to_string(prod.join("rules/r1.py")).unwrap(), "prod_condition\n");
    // Un-shadowed sidecar is copied from source verbatim.
    assert_eq!(std::fs::read_to_string(prod.join("hooks/other.py")).unwrap(), "def g(p):\n    return 'keep'\n");
}

#[test]
fn migrate_overlay_dangling_shadow_is_a_hard_error_and_writes_nothing() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");
    write(&test_root.join("hooks/extractor.json"), &serde_json::json!({ "name": "Extractor", "type": "function" }));
    std::fs::write(test_root.join("hooks/extractor.py"), b"x\n").unwrap();

    // Shadow for a hook that does not exist in the source.
    let ov = root.join("envs/prod/overlay/hooks");
    std::fs::create_dir_all(&ov).unwrap();
    std::fs::write(ov.join("ghost.py"), b"x\n").unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();

    let err = result.unwrap_err().to_string();
    assert!(err.contains("hooks/ghost.py"), "error names the offending shadow: {err}");
    // Fail-fast: no target sidecar was written.
    assert!(!root.join("envs/prod/hooks/extractor.py").exists(), "must not write before validating");
}

#[test]
fn migrate_overlay_json_shadow_is_rejected() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");
    write(&test_root.join("hooks/extractor.json"), &serde_json::json!({ "name": "Extractor", "type": "function" }));
    std::fs::write(test_root.join("hooks/extractor.py"), b"x\n").unwrap();

    // A JSON shadow — out of scope (JSON is overlay.toml's job).
    let ov = root.join("envs/prod/overlay/hooks");
    std::fs::create_dir_all(&ov).unwrap();
    std::fs::write(ov.join("extractor.json"), b"{}\n").unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();

    let err = result.unwrap_err().to_string();
    assert!(err.contains("hooks/extractor.json"), "json shadow is rejected: {err}");
}

#[test]
fn migrate_overlay_only_excluded_shadow_does_not_error() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");
    write(&test_root.join("hooks/a.json"), &serde_json::json!({ "name": "A", "type": "function" }));
    std::fs::write(test_root.join("hooks/a.py"), b"a\n").unwrap();
    write(&test_root.join("hooks/b.json"), &serde_json::json!({ "name": "B", "type": "function" }));
    std::fs::write(test_root.join("hooks/b.py"), b"b\n").unwrap();

    // Valid shadow for hooks/b, but this run scopes to hooks/a via --only.
    let ov = root.join("envs/prod/overlay/hooks");
    std::fs::create_dir_all(&ov).unwrap();
    std::fs::write(ov.join("b.py"), b"b-prod\n").unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec!["hooks/a".to_string()], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("a valid shadow for an --only-excluded sidecar must not error");

    let prod = root.join("envs/prod");
    assert!(prod.join("hooks/a.json").exists(), "selected object migrated");
    assert!(!prod.join("hooks/b.json").exists(), "excluded object not migrated");
}

/// Promotion case: the workspace/queue slugs are renamed, so the shadow lives
/// at the REMAPPED (target) path. Both `produced_sidecars` and the shadow
/// lookup key off `remap_relative`, so the override must apply at the renamed
/// path — the most error-prone real-world scenario.
#[test]
fn migrate_overlay_shadow_applies_at_renamed_target_path() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");

    write(
        &test_root.join("workspaces/main/workspace.json"),
        &serde_json::json!({ "name": "Main" }),
    );
    write(
        &test_root.join("workspaces/main/queues/invoices/queue.json"),
        &serde_json::json!({ "name": "Invoices" }),
    );
    write(
        &test_root.join("workspaces/main/queues/invoices/schema.json"),
        &serde_json::json!({ "name": "S", "content": [] }),
    );
    let src_formula = test_root.join("workspaces/main/queues/invoices/formulas/export_path.py");
    std::fs::create_dir_all(src_formula.parent().unwrap()).unwrap();
    std::fs::write(&src_formula, b"\"/test/exports\"\n").unwrap();

    // Rename main -> main-prod, invoices -> invoices-prod.
    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        map_dir.join("test-to-prod.toml"),
        "version = 1\n\n[workspaces]\n\"main\" = \"main-prod\"\n\n[queues]\n\"invoices\" = \"invoices-prod\"\n\n[schemas]\n\"invoices\" = \"invoices-prod\"\n",
    )
    .unwrap();

    // Shadow placed at the REMAPPED (target) path.
    let shadow = root
        .join("envs/prod/overlay/workspaces/main-prod/queues/invoices-prod/formulas/export_path.py");
    std::fs::create_dir_all(shadow.parent().unwrap()).unwrap();
    std::fs::write(&shadow, b"\"/prod/exports\"\n").unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let prod_formula =
        root.join("envs/prod/workspaces/main-prod/queues/invoices-prod/formulas/export_path.py");
    assert_eq!(
        std::fs::read_to_string(&prod_formula).unwrap(),
        "\"/prod/exports\"\n",
        "shadow at the remapped target path must be applied"
    );
}

/// The shadow mechanism is extension-agnostic: a Node.js hook's `.js` sidecar
/// is overridden just like a `.py` one.
#[test]
fn migrate_overlay_shadow_replaces_nodejs_hook_js_sidecar() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");

    write(
        &test_root.join("hooks/webhook.json"),
        &serde_json::json!({ "name": "Webhook", "type": "function", "config": { "runtime": "nodejs20.x" } }),
    );
    std::fs::write(test_root.join("hooks/webhook.js"), b"// src\n").unwrap();

    let shadow = root.join("envs/prod/overlay/hooks/webhook.js");
    std::fs::create_dir_all(shadow.parent().unwrap()).unwrap();
    std::fs::write(&shadow, b"// prod\n").unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    assert_eq!(
        std::fs::read_to_string(root.join("envs/prod/hooks/webhook.js")).unwrap(),
        "// prod\n",
        "the .js hook sidecar shadow must be applied"
    );
}

// --- un-creatable duplicate unique-typed email templates ------------------
//
// The Rossum API enforces per-queue uniqueness for certain email-template
// types (`rejection_default`, `email_with_no_processable_attachments`): a
// queue can hold at most ONE template of such a type, and creating a second
// returns `400 Cannot create template with unique type: <type>`. A source env
// can still carry historical duplicates (created server-side before the
// constraint). Mirroring those into a target snapshot plants files that
// `rdc sync` will POST → 400 → warn on EVERY run, forever. Migrate must
// instead keep only the copies the target can actually hold.

/// Helper: write a minimal email template into `envs/<env>/workspaces/main/
/// queues/invoices/email-templates/<slug>.json`.
fn write_template(root: &std::path::Path, env: &str, slug: &str, id: u64, ty: &str) {
    write(
        &root.join(format!(
            "envs/{env}/workspaces/main/queues/invoices/email-templates/{slug}.json"
        )),
        &serde_json::json!({
            "id": id,
            "name": "Default rejection template",
            "type": ty,
            "queue": "rdc://queues/invoices",
            "url": format!("rdc://email_templates/main/invoices/{slug}"),
        }),
    );
}

/// Helper: write a target-env lockfile granting remote identity to the given
/// email-template compound keys (`<ws>/<q>/<slug>`).
fn write_tgt_lockfile(root: &std::path::Path, env: &str, keys: &[&str]) {
    let mut templates = serde_json::Map::new();
    for (i, key) in keys.iter().enumerate() {
        templates.insert(
            key.to_string(),
            serde_json::json!({"id": 500 + i as u64, "modified_at": null, "content_hash": "x"}),
        );
    }
    let lf = serde_json::json!({
        "version": 3,
        "api_base": "https://prod.example/api/v1",
        "objects": { "email_templates": templates },
    });
    write(&root.join(format!(".rdc/state/{env}.lock.json")), &lf);
}

/// Source queue holds TWO `rejection_default` templates (a historical
/// duplicate); the target only has remote identity for the first. The second
/// is un-creatable on the target (unique type) → migrate must NOT produce it,
/// and `--mirror` must prune a stale copy left by an earlier migrate.
#[test]
fn migrate_mirror_skips_second_unique_typed_template_per_queue() {
    let project = init_two_env_project();
    let root = project.path();

    write_template(root, "test", "default-rejection-template", 100, "rejection_default");
    write_template(root, "test", "default-rejection-template-2", 200, "rejection_default");
    // Target lockfile: only the plain slug exists remotely on prod.
    write_tgt_lockfile(root, "prod", &["main/invoices/default-rejection-template"]);
    // Stale copy of the duplicate from an earlier (pre-fix) migrate.
    write_template(root, "prod", "default-rejection-template-2", 0, "rejection_default");

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", true, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate --mirror should succeed");

    assert!(
        root.join("envs/prod/workspaces/main/queues/invoices/email-templates/default-rejection-template.json").exists(),
        "the identity-backed template must be migrated"
    );
    assert!(
        !root.join("envs/prod/workspaces/main/queues/invoices/email-templates/default-rejection-template-2.json").exists(),
        "the un-creatable duplicate must be skipped and its stale copy pruned"
    );
}

/// When the target holds BOTH duplicates remotely (its own historical pair),
/// both are patchable — migrate must keep mirroring both.
#[test]
fn migrate_keeps_unique_typed_duplicates_that_exist_on_target() {
    let project = init_two_env_project();
    let root = project.path();

    write_template(root, "test", "default-rejection-template", 100, "rejection_default");
    write_template(root, "test", "default-rejection-template-2", 200, "rejection_default");
    write_tgt_lockfile(
        root,
        "prod",
        &[
            "main/invoices/default-rejection-template",
            "main/invoices/default-rejection-template-2",
        ],
    );

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", true, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate --mirror should succeed");

    assert!(
        root.join("envs/prod/workspaces/main/queues/invoices/email-templates/default-rejection-template.json").exists()
    );
    assert!(
        root.join("envs/prod/workspaces/main/queues/invoices/email-templates/default-rejection-template-2.json").exists(),
        "a duplicate the target actually holds (lockfile identity) must keep mirroring"
    );
}

/// Fresh target (no lockfile identity for either): exactly ONE of the
/// duplicates is creatable — keep the lowest source id, deterministically.
#[test]
fn migrate_keeps_lowest_id_unique_typed_duplicate_on_fresh_target() {
    let project = init_two_env_project();
    let root = project.path();

    // The `-2` slug carries the LOWER id: the keep-rule is id-based, not
    // slug-suffix-based.
    write_template(root, "test", "default-rejection-template", 200, "rejection_default");
    write_template(root, "test", "default-rejection-template-2", 100, "rejection_default");

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", true, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate --mirror should succeed");

    assert!(
        root.join("envs/prod/workspaces/main/queues/invoices/email-templates/default-rejection-template-2.json").exists(),
        "the lowest-id duplicate must be kept"
    );
    assert!(
        !root.join("envs/prod/workspaces/main/queues/invoices/email-templates/default-rejection-template.json").exists(),
        "the higher-id duplicate must be skipped (only one is creatable)"
    );
}

/// Non-unique types (e.g. `custom`) may repeat on a queue — duplicates of
/// those must migrate untouched.
#[test]
fn migrate_leaves_custom_type_duplicates_alone() {
    let project = init_two_env_project();
    let root = project.path();

    write_template(root, "test", "status-change", 100, "custom");
    write_template(root, "test", "status-change-2", 200, "custom");

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", true, false, vec![], true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate --mirror should succeed");

    assert!(
        root.join("envs/prod/workspaces/main/queues/invoices/email-templates/status-change.json").exists()
    );
    assert!(
        root.join("envs/prod/workspaces/main/queues/invoices/email-templates/status-change-2.json").exists(),
        "custom-type duplicates are creatable and must both migrate"
    );
}
