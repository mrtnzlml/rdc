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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, true, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, true, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec!["hooks/keeper".into()], true, false);
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
        false,
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
        false,
    );
    std::env::set_current_dir(&prev).unwrap();

    let err = result.expect_err("an --only selector matching nothing must abort");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("matched 0 objects"),
        "error must explain the selector matched nothing: {msg}"
    );
}

/// A manual dataset must promote whole: the flag in `collection.json` and the
/// rows in `data.jsonl` both land in the target env, byte-identical. Without
/// `jsonl` as a managed leaf the rows are silently dropped and replication is a
/// no-op that looks like success.
#[test]
fn migrate_carries_manual_mdh_dataset_rows() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");

    write(
        &test_root.join("mdh/gl-codes/collection.json"),
        &serde_json::json!({ "name": "GL_CODES", "data": "manual" }),
    );
    write(
        &test_root.join("mdh/gl-codes/indexes.json"),
        &serde_json::json!({ "regular": [], "search": [] }),
    );
    let rows = "{\"code\":\"1000\",\"label\":\"Office supplies\"}\n\
                {\"code\":\"2000\",\"label\":\"Travel\"}\n";
    std::fs::create_dir_all(test_root.join("mdh/gl-codes")).unwrap();
    std::fs::write(test_root.join("mdh/gl-codes/data.jsonl"), rows).unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let prod = root.join("envs/prod/mdh/gl-codes");
    assert_eq!(
        std::fs::read_to_string(prod.join("data.jsonl")).unwrap(),
        rows,
        "row data must migrate byte-identically"
    );
    assert_eq!(
        read_json(&prod.join("collection.json"))["data"],
        serde_json::json!("manual"),
        "the opt-in flag must survive the migration"
    );
}

/// `--only mdh/<slug>` must select the whole dataset — manifest, indexes, and
/// rows — and nothing else. Before S1 this selector bailed with "unknown kind"
/// because mdh was absent from DEPLOYABLE_KINDS.
#[test]
fn migrate_only_selects_a_whole_mdh_dataset() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");

    for slug in ["gl-codes", "synonyms"] {
        write(
            &test_root.join(format!("mdh/{slug}/collection.json")),
            &serde_json::json!({ "name": slug, "data": "manual" }),
        );
        write(
            &test_root.join(format!("mdh/{slug}/indexes.json")),
            &serde_json::json!({ "regular": [], "search": [] }),
        );
        std::fs::write(
            test_root.join(format!("mdh/{slug}/data.jsonl")),
            b"{\"code\":\"1000\"}\n",
        )
        .unwrap();
    }

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run(
        "test",
        "prod",
        false,
        false,
        vec!["mdh/gl-codes".into()],
        true,
        false,
    );
    std::env::set_current_dir(&prev).unwrap();
    result.expect("--only mdh/<slug> should succeed");

    let prod = root.join("envs/prod/mdh");
    for leaf in ["collection.json", "indexes.json", "data.jsonl"] {
        assert!(
            prod.join("gl-codes").join(leaf).exists(),
            "{leaf} of the selected dataset must be migrated"
        );
    }
    assert!(
        !prod.join("synonyms").exists(),
        "an unselected dataset must NOT be migrated"
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
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

/// Determinism: migrating the same source snapshot twice must produce
/// byte-identical target files. The first run CREATES the target object (no
/// target file, so `reconcile_target_identity` strips the universal server
/// fields); the second run sees the file it just wrote and takes the MATCHED
/// path (restores/removes the env fields). Those two paths remove a different
/// set of keys, and JSON object key removal must not reorder the surviving
/// keys — otherwise the on-disk bytes flip between runs (non-deterministic,
/// noisy `git diff`) even though the content is identical. Regression for the
/// `Map::remove` (swap-remove) key-scramble in the migrate transform.
#[test]
fn migrate_inbox_byte_deterministic_across_create_then_update() {
    let project = init_two_env_project();
    let root = project.path();

    // SOURCE (test) inbox with env-specific fields (id/url/email/modified_by)
    // interleaved with kept deployable fields, matching a real pulled inbox.
    write(
        &root.join("envs/test/workspaces/main/workspace.json"),
        &serde_json::json!({ "name": "Main" }),
    );
    write(
        &root.join("envs/test/workspaces/main/queues/invoices/inbox.json"),
        &serde_json::json!({
            "id": 10,
            "url": "https://test.example/api/v1/inboxes/10",
            "name": "Invoices Inbox",
            "email": "invoices@test.example.rossum.app",
            "queues": ["rdc://queues/invoices"],
            "email_prefix": "invoices",
            "bounce_email_to": null,
            "metadata": {},
            "filters": { "allowed_senders": [] },
            "dmarc_check_action": "accept",
            "modified_by": "https://test.example/api/v1/users/1"
        }),
    );

    // A source lockfile carrying the env's api_base makes migrate run its
    // source-host cleanup: the inbox `email` (whose domain is the source host)
    // is dropped on the CREATE path but is already absent from the target on
    // the UPDATE path — so the two paths strip a different key set. Without a
    // deterministic write order this flips the surviving keys' order between
    // runs. (This mirrors a real pulled snapshot, where the lockfile always
    // records api_base.)
    write(
        &root.join(".rdc/state/test.lock.json"),
        &serde_json::json!({
            "version": 3,
            "api_base": "https://test.example/api/v1",
            "objects": {}
        }),
    );

    let inbox = root.join("envs/prod/workspaces/main/queues/invoices/inbox.json");

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();

    // First migrate: target inbox does not exist yet → CREATE path.
    rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false)
        .expect("first migrate should succeed");
    let after_create = std::fs::read(&inbox).expect("inbox written by first migrate");

    // Second migrate: target inbox now exists → MATCHED/UPDATE path.
    rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false)
        .expect("second migrate should succeed");
    let after_update = std::fs::read(&inbox).expect("inbox present after second migrate");

    std::env::set_current_dir(&prev).unwrap();

    assert_eq!(
        after_create,
        after_update,
        "migrate must be byte-deterministic: create-path and update-path output differ.\n\
         CREATE:\n{}\nUPDATE:\n{}",
        String::from_utf8_lossy(&after_create),
        String::from_utf8_lossy(&after_update),
    );
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], false, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
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

/// Helper: src+tgt inbox trees under an identity mapping, each with its own
/// `email_prefix`. Pass `None` for `tgt_prefix` to leave the target inbox
/// without one, or for `tgt` to omit the target inbox entirely (new object).
fn setup_inbox_prefix_project(src_prefix: &str, tgt_prefix: Option<Option<&str>>) -> TempDir {
    let project = init_two_env_project();
    let root = project.path().to_path_buf();
    for env in ["test", "prod"] {
        write(
            &root.join(format!("envs/{env}/workspaces/main/workspace.json")),
            &serde_json::json!({ "name": "Main" }),
        );
        write(
            &root.join(format!("envs/{env}/workspaces/main/queues/invoices/queue.json")),
            &serde_json::json!({ "name": "Invoices", "workspace": "rdc://workspaces/main" }),
        );
    }
    write(
        &root.join("envs/test/workspaces/main/queues/invoices/inbox.json"),
        &serde_json::json!({
            "name": "Invoices Inbox",
            "queues": ["rdc://queues/invoices"],
            "email_prefix": src_prefix,
        }),
    );
    if let Some(tgt) = tgt_prefix {
        let mut body = serde_json::json!({
            "name": "Invoices Inbox",
            "queues": ["rdc://queues/invoices"],
        });
        if let Some(p) = tgt {
            body["email_prefix"] = serde_json::json!(p);
        }
        write(
            &root.join("envs/prod/workspaces/main/queues/invoices/inbox.json"),
            &body,
        );
    }
    project
}

/// Default (no `--migrate-email-prefixes`): a matched target inbox keeps its
/// OWN `email_prefix`. Carrying the source's would re-address the target's
/// public mailbox (`email` is `<email_prefix>-<hash>@<host>`) and break mail
/// sent to the old address.
#[test]
fn migrate_ignores_inbox_email_prefix_by_default() {
    let project = setup_inbox_prefix_project("acme-dev--ops", Some(Some("acme")));
    let root = project.path();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], false, false);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let inbox = read_json(&root.join("envs/prod/workspaces/main/queues/invoices/inbox.json"));
    assert_eq!(
        inbox["email_prefix"],
        serde_json::json!("acme"),
        "matched target must keep its own email_prefix"
    );
}

/// A brand-new target inbox KEEPS the source's `email_prefix`. The field is
/// mandatory on create — `POST /inboxes` answers `400 non_field_errors: One of
/// fields 'email_prefix' or 'email' needs to be provided`, and rdc strips the
/// server-assigned `email` — so dropping it (as the score-threshold reconcile
/// this was modelled on does) emitted an object no sync could ever push. A
/// created mailbox has no senders to strand, so inheriting is safe here in a
/// way overwriting a live target's prefix is not; migrate warns about each one.
#[test]
fn migrate_keeps_source_inbox_email_prefix_for_new_object() {
    let project = setup_inbox_prefix_project("acme-dev--ops", None);
    let root = project.path();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], false, false);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let inbox = read_json(&root.join("envs/prod/workspaces/main/queues/invoices/inbox.json"));
    assert_eq!(
        inbox["email_prefix"],
        serde_json::json!("acme-dev--ops"),
        "a new target inbox must stay pushable, so it keeps the source's email_prefix: {inbox}"
    );
}

/// The same when the target inbox file exists but carries no prefix — the state
/// every migrate between the drop's introduction and this fix left behind. It
/// is still a create (nothing in the target lockfile), so it must be repaired
/// rather than left permanently unpushable.
#[test]
fn migrate_fills_missing_email_prefix_on_an_undeployed_target_inbox() {
    let project = setup_inbox_prefix_project("acme-dev--ops", Some(None));
    let root = project.path();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], false, false);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let inbox = read_json(&root.join("envs/prod/workspaces/main/queues/invoices/inbox.json"));
    assert_eq!(
        inbox["email_prefix"],
        serde_json::json!("acme-dev--ops"),
        "a prefix-less, undeployed target inbox must be repaired, not left unpushable: {inbox}"
    );
}

/// With `--migrate-email-prefixes`, migrate carries the SOURCE's prefix
/// verbatim (the pre-existing behavior, now opt-in).
#[test]
fn migrate_carries_inbox_email_prefix_with_flag() {
    let project = setup_inbox_prefix_project("acme-dev--ops", Some(Some("acme")));
    let root = project.path();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], false, true);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let inbox = read_json(&root.join("envs/prod/workspaces/main/queues/invoices/inbox.json"));
    assert_eq!(
        inbox["email_prefix"],
        serde_json::json!("acme-dev--ops"),
        "opting in must carry the source email_prefix"
    );
}

/// The target's `overlay.toml` still wins: it is applied before the reconcile,
/// and an explicitly declared prefix is the user stating the target's address
/// on purpose — the reconcile must not overwrite it with the pulled value.
#[test]
fn migrate_inbox_email_prefix_overlay_wins() {
    let project = setup_inbox_prefix_project("acme-dev--ops", Some(Some("acme")));
    let root = project.path();
    std::fs::write(
        root.join("envs/prod/overlay.toml"),
        "version = 1\n\n[inboxes.invoices]\nemail_prefix = \"acme-prod\"\n",
    )
    .unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], false, false);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let inbox = read_json(&root.join("envs/prod/workspaces/main/queues/invoices/inbox.json"));
    assert_eq!(
        inbox["email_prefix"],
        serde_json::json!("acme-prod"),
        "an explicit overlay email_prefix must win over the reconcile"
    );
}

/// The same precedence rule for the OTHER reconciled fields. `overlay.toml` is
/// the user declaring the target's value on purpose, and migrate documents
/// "per-object override > kind-wide default > reconciled value" — so a
/// reconcile must never overwrite a field the overlay explicitly set. Without
/// this, an overlay entry for one of these keys is silently inert forever: the
/// reconcile writes the target's pulled value back on every run.
#[test]
fn migrate_overlay_wins_over_score_threshold_reconcile() {
    let project = setup_threshold_project(0.5, 0.9, 0.5, 0.85);
    let root = project.path();
    std::fs::write(
        root.join("envs/prod/overlay.toml"),
        "version = 1\n\n[queues.invoices]\nsettings.default_score_threshold = 0.7\n",
    )
    .unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], false, false);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let queue = read_json(&root.join("envs/prod/workspaces/main/queues/invoices/queue.json"));
    assert_eq!(
        queue["settings"]["default_score_threshold"],
        serde_json::json!(0.7),
        "an explicit overlay default_score_threshold must win over the reconcile"
    );
}

#[test]
fn migrate_overlay_wins_over_training_enabled_reconcile() {
    let project = init_two_env_project();
    let root = project.path();
    for (env, training) in [("test", true), ("prod", false)] {
        write(
            &root.join(format!("envs/{env}/workspaces/main/workspace.json")),
            &serde_json::json!({ "name": "Main" }),
        );
        write(
            &root.join(format!("envs/{env}/workspaces/main/queues/invoices/queue.json")),
            &serde_json::json!({
                "name": "Invoices",
                "workspace": "rdc://workspaces/main",
                "training_enabled": training,
            }),
        );
    }
    std::fs::write(
        root.join("envs/prod/overlay.toml"),
        "version = 1\n\n[queues.invoices]\ntraining_enabled = true\n",
    )
    .unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], false, false);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let queue = read_json(&root.join("envs/prod/workspaces/main/queues/invoices/queue.json"));
    assert_eq!(
        queue["training_enabled"],
        serde_json::json!(true),
        "an explicit overlay training_enabled must win over the reconcile"
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
    rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false).expect("migrate ok");
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec!["hooks/a".to_string()], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", true, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", true, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", true, false, vec![], true, false);
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
    let result = rdc::cli::migrate::run("test", "prod", true, false, vec![], true, false);
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

/// Capture a migrate run's stderr (where the CLI logs) so warnings/summaries
/// can be asserted. Uses the binary (a subprocess), so — unlike the in-process
/// `migrate::run` tests — it does not touch the process cwd and needs no
/// `cwd_lock`.
fn migrate_stderr(root: &std::path::Path, args: &[&str]) -> String {
    let mut argv = vec!["migrate"];
    argv.extend_from_slice(args);
    let assert = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(&argv)
        .assert()
        .success();
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

/// [3] A malformed legacy `.rdc/map/*.toml` unrelated to the requested
/// direction must NOT abort the one-time conversion (before the generic file
/// existed, `migrate <src> <tgt>` only read its own pair file). The good pair
/// converts and is deleted; the malformed file is WARNED about and LEFT on disk
/// so the mapping it encodes is never silently destroyed.
#[test]
fn migrate_skips_malformed_legacy_file_and_converts_the_good_ones() {
    let project = TempDir::new().unwrap();
    let root = project.path();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args([
            "init",
            "--env",
            "test=https://test.example/api/v1:1",
            "--env",
            "prod=https://prod.example/api/v1:2",
            "--env",
            "dev=https://dev.example/api/v1:3",
        ])
        .assert()
        .success();

    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    // Good, well-formed legacy pair with a real rename.
    std::fs::write(
        map_dir.join("test-to-prod.toml"),
        "version = 1\n\n[queues]\n\"invoices\" = \"invoices-prod\"\n",
    )
    .unwrap();
    // Unrelated MALFORMED file whose pair IS in rdc.toml, so it reaches the
    // `Mapping::load` that used to `?`-abort the whole conversion.
    std::fs::write(map_dir.join("test-to-dev.toml"), "this is not valid toml {{{").unwrap();

    let stderr = migrate_stderr(root, &["test", "prod"]);

    assert!(
        stderr.contains("skipping malformed legacy mapping file"),
        "must warn about + skip the malformed file, not abort: {stderr}"
    );
    let generic = std::fs::read_to_string(root.join(".rdc/mapping.toml"))
        .expect(".rdc/mapping.toml must be written from the good pair");
    assert!(
        generic.contains("invoices") && generic.contains("invoices-prod"),
        "the good rename must land in mapping.toml: {generic}"
    );
    assert!(
        !map_dir.join("test-to-prod.toml").exists(),
        "the converted good legacy file must be deleted"
    );
    assert!(
        map_dir.join("test-to-dev.toml").exists(),
        "the malformed legacy file must be LEFT in place, never deleted"
    );
}

/// [8] With a committed `.rdc/mapping.toml`, legacy `.rdc/map/*.toml` files are
/// never read — but a lingering one (reappeared via a branch/merge, or left by
/// a malformed-skip) must be WARNED about, not silently ignored, since the
/// renames it encodes are being dropped. The file is not modified.
#[test]
fn migrate_warns_about_leftover_legacy_files_beside_generic_mapping() {
    let project = init_two_env_project();
    let root = project.path();
    std::fs::create_dir_all(root.join(".rdc")).unwrap();
    std::fs::write(root.join(".rdc/mapping.toml"), "version = 2\n").unwrap();
    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        map_dir.join("test-to-prod.toml"),
        "version = 1\n\n[queues]\n\"invoices\" = \"invoices-prod\"\n",
    )
    .unwrap();

    let stderr = migrate_stderr(root, &["test", "prod"]);

    assert!(
        stderr.contains("are present alongside") && stderr.contains("IGNORED"),
        "must warn that leftover legacy files are ignored: {stderr}"
    );
    assert!(
        map_dir.join("test-to-prod.toml").exists(),
        "the leftover legacy file must not be deleted"
    );
}

/// [11] The migrate summary counts workflow/MDH objects, which `classify`
/// ignores. Migrating a source that holds only a workflow into an empty target
/// must report `1 create`, not `0` (the pre-fix undercount).
#[test]
fn migrate_summary_counts_workflow_objects() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/workflows/ap-flow/workflow.json"),
        &serde_json::json!({ "name": "AP Flow", "type": "approval" }),
    );

    let stderr = migrate_stderr(root, &["test", "prod"]);

    assert!(
        stderr.contains("1 create"),
        "the workflow object must be counted as a create: {stderr}"
    );
    assert!(
        root.join("envs/prod/workflows/ap-flow/workflow.json").exists(),
        "the workflow must actually be migrated to the target"
    );
}

/// Stale-source guardrail: a `.rdc/mapping.toml` row whose SOURCE slug names no
/// object on disk must WARN (otherwise the intended rename silently never
/// applies, indistinguishable from a typo).
#[test]
fn migrate_warns_when_mapping_source_slug_is_absent() {
    let project = init_two_env_project();
    let root = project.path();
    std::fs::create_dir_all(root.join(".rdc")).unwrap();
    std::fs::write(
        root.join(".rdc/mapping.toml"),
        "version = 2\n\n[[queues]]\ntest = \"ghost\"\nprod = \"ghost-prod\"\n",
    )
    .unwrap();

    let stderr = migrate_stderr(root, &["test", "prod"]);

    assert!(
        stderr.contains("mapping entry queues/ghost has no matching object"),
        "must warn that the stale mapping source slug has no object: {stderr}"
    );
}

/// [4] A genuine cross-file slug conflict (one env forced to two slugs) is
/// unrepresentable in a single N-way file, so the one-time conversion ABORTS
/// rather than silently pick a winner — but the error names the legacy files
/// being converted (actionable) and nothing is written or deleted (no data
/// lost). `migrate dev prod` aborting on a conflict that also involves `test`
/// is the deliberate cost of a global, faithful conversion.
#[test]
fn migrate_aborts_with_file_attribution_on_inconsistent_legacy_files() {
    let project = TempDir::new().unwrap();
    let root = project.path();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args([
            "init",
            "--env",
            "test=https://test.example/api/v1:1",
            "--env",
            "prod=https://prod.example/api/v1:2",
            "--env",
            "dev=https://dev.example/api/v1:3",
        ])
        .assert()
        .success();

    // dev->prod: x->y ; dev->test: x->z ; prod->test: y->w — object x's node is
    // forced to test=z AND (via y) test=w: inconsistent.
    let map_dir = root.join(".rdc/map");
    std::fs::create_dir_all(&map_dir).unwrap();
    std::fs::write(
        map_dir.join("dev-to-prod.toml"),
        "version = 1\n\n[queues]\n\"x\" = \"y\"\n",
    )
    .unwrap();
    std::fs::write(
        map_dir.join("dev-to-test.toml"),
        "version = 1\n\n[queues]\n\"x\" = \"z\"\n",
    )
    .unwrap();
    std::fs::write(
        map_dir.join("prod-to-test.toml"),
        "version = 1\n\n[queues]\n\"y\" = \"w\"\n",
    )
    .unwrap();

    let out = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "dev", "prod"])
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "an inconsistent conversion must abort (non-zero exit)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("inconsistent"),
        "error must explain the inconsistency: {stderr}"
    );
    assert!(
        stderr.contains("converting legacy mapping files"),
        "error must attribute the legacy files being converted: {stderr}"
    );
    // Nothing written or deleted on the abort.
    assert!(
        map_dir.join("dev-to-prod.toml").exists(),
        "legacy files must be preserved when conversion aborts"
    );
    assert!(
        !root.join(".rdc/mapping.toml").exists(),
        "no .rdc/mapping.toml may be written when conversion aborts"
    );
}

/// Write a two-file hook (JSON + `.py` sidecar) into `env`'s snapshot.
fn write_hook(root: &std::path::Path, env: &str, slug: &str, code: &str) {
    write(
        &root.join(format!("envs/{env}/hooks/{slug}.json")),
        &serde_json::json!({
            "name": "Extractor",
            "type": "function",
            "events": ["annotation_content.initialize"],
            "config": { "runtime": "python3.12" },
        }),
    );
    std::fs::write(root.join(format!("envs/{env}/hooks/{slug}.py")), code).unwrap();
}

/// The summary must describe what the run DID, not how big the snapshot is. A
/// repeat migrate over an already-mirrored target writes nothing, so it must
/// report zero changed files and zero updates — the pre-fix output counted every
/// file it *considered* ("2 file(s)") and every target file that merely *existed*
/// ("1 update"), which is indistinguishable from a run that rewrote everything.
#[test]
fn migrate_summary_reports_nothing_changed_on_a_repeat_run() {
    let project = init_two_env_project();
    let root = project.path();
    write_hook(root, "test", "extractor", "def f(): pass\n");

    let first = migrate_stderr(root, &["test", "prod"]);
    assert!(
        first.contains("2 of 2 file(s) changed"),
        "the first migrate writes both files: {first}"
    );
    assert!(
        first.contains("1 create"),
        "the first migrate creates the hook object: {first}"
    );

    let second = migrate_stderr(root, &["test", "prod"]);
    assert!(
        second.contains("0 of 2 file(s) changed"),
        "a repeat migrate writes nothing and must say so: {second}"
    );
    assert!(
        second.contains("0 update") && second.contains("1 unchanged"),
        "an object whose files are byte-identical is unchanged, not updated: {second}"
    );
}

/// An object counts as `update` only when its produced content actually differs.
/// A change confined to a `.py` sidecar still updates the owning hook object —
/// per-object status aggregates over the object's JSON *and* its sidecars.
#[test]
fn migrate_summary_counts_a_sidecar_only_change_as_one_update() {
    let project = init_two_env_project();
    let root = project.path();
    write_hook(root, "test", "extractor", "def f(): pass\n");
    migrate_stderr(root, &["test", "prod"]);

    // Touch only the source sidecar; the hook JSON stays byte-identical.
    std::fs::write(
        root.join("envs/test/hooks/extractor.py"),
        "def f(): return 1\n",
    )
    .unwrap();

    let stderr = migrate_stderr(root, &["test", "prod"]);

    assert!(
        stderr.contains("1 of 2 file(s) changed"),
        "only the sidecar is rewritten: {stderr}"
    );
    assert!(
        stderr.contains("1 update") && stderr.contains("0 unchanged"),
        "the owning hook object must count as exactly one update: {stderr}"
    );
}

/// `--dry-run` must forecast the same counts the real run reports, otherwise the
/// plan cannot be used to decide whether to run it for real.
#[test]
fn migrate_dry_run_forecasts_the_same_counts_as_the_real_run() {
    let project = init_two_env_project();
    let root = project.path();
    write_hook(root, "test", "extractor", "def f(): pass\n");
    migrate_stderr(root, &["test", "prod"]);

    let dry = migrate_stderr(root, &["test", "prod", "--dry-run"]);

    assert!(
        dry.contains("0 of 2 file(s) changed"),
        "dry-run over a mirrored target must forecast no writes: {dry}"
    );
    assert!(
        dry.contains("0 update") && dry.contains("1 unchanged"),
        "dry-run must forecast the same per-object status: {dry}"
    );
}

/// `--mirror` prunes target-only object FILES; the directories they occupied
/// must go too. git cannot represent an empty directory, so a leftover
/// `mdh/<slug>/` is invisible in the `git diff` review the command tells the
/// user to perform — yet it stays on disk, where every filesystem-level view of
/// the snapshot still shows a dataset that no longer exists.
#[test]
fn migrate_mirror_prune_removes_the_directory_it_emptied() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/mdh/vendors/indexes.json"),
        &serde_json::json!([{ "name": "by_id", "key": { "id": 1 } }]),
    );
    // Target-only dataset: `--mirror` must prune both of its files.
    write(
        &root.join("envs/prod/mdh/orphan/collection.json"),
        &serde_json::json!({ "name": "ORPHAN" }),
    );
    write(
        &root.join("envs/prod/mdh/orphan/indexes.json"),
        &serde_json::json!([]),
    );

    let stderr = migrate_stderr(root, &["test", "prod", "--mirror"]);

    assert!(
        !root.join("envs/prod/mdh/orphan/indexes.json").exists(),
        "the target-only dataset's files must be pruned: {stderr}"
    );
    assert!(
        !root.join("envs/prod/mdh/orphan").exists(),
        "the emptied dataset directory must be removed too, not left behind: {stderr}"
    );
    assert!(
        stderr.contains("prune dir mdh/orphan"),
        "the removal must be LOGGED — `git diff` can never show it: {stderr}"
    );
    assert!(
        root.join("envs/prod/mdh/vendors/indexes.json").exists(),
        "the mirrored dataset must survive: {stderr}"
    );
}

/// Empty-directory cleanup must never reach a directory that still holds a file
/// rdc does not manage. A foreign file (an old sync shadow artifact, an editor
/// leftover) is the user's, so its directory stays — pruning the managed objects
/// around it must not delete it as collateral.
#[test]
fn migrate_mirror_keeps_a_pruned_directory_that_still_holds_a_foreign_file() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/mdh/vendors/indexes.json"),
        &serde_json::json!([]),
    );
    write(
        &root.join("envs/prod/mdh/orphan/indexes.json"),
        &serde_json::json!([]),
    );
    // Foreign, unmanaged leaf living beside the pruned object.
    std::fs::write(root.join("envs/prod/mdh/orphan/notes.txt"), b"keep me").unwrap();

    let stderr = migrate_stderr(root, &["test", "prod", "--mirror"]);

    assert!(
        !root.join("envs/prod/mdh/orphan/indexes.json").exists(),
        "the managed object must still be pruned: {stderr}"
    );
    assert!(
        root.join("envs/prod/mdh/orphan/notes.txt").exists(),
        "an unmanaged file must never be deleted: {stderr}"
    );
    assert!(
        root.join("envs/prod/mdh/orphan").exists(),
        "a directory that still holds a file must be kept: {stderr}"
    );
}

/// `--mirror --dry-run` must name the directories it would remove. They are the
/// one part of the plan `git diff` can never show afterwards.
#[test]
fn migrate_mirror_dry_run_plans_the_directory_removal() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/mdh/vendors/indexes.json"),
        &serde_json::json!([]),
    );
    write(
        &root.join("envs/prod/mdh/orphan/indexes.json"),
        &serde_json::json!([]),
    );

    let stderr = migrate_stderr(root, &["test", "prod", "--mirror", "--dry-run"]);

    assert!(
        stderr.contains("prune dir mdh/orphan"),
        "the plan must name the directory it would remove, distinctly from the \
         files inside it: {stderr}"
    );
    assert!(
        root.join("envs/prod/mdh/orphan/indexes.json").exists(),
        "--dry-run must not delete anything: {stderr}"
    );
    assert!(
        root.join("envs/prod/mdh/orphan").exists(),
        "--dry-run must not remove the directory either: {stderr}"
    );
}

// ---- organization promotion ------------------------------------------

/// Promotion carries `settings` and leaves the target's own identity alone.
#[test]
fn migrate_promotes_organization_settings_only() {
    let project = init_two_env_project();
    let root = project.path();

    write(
        &root.join("envs/test/organization.json"),
        &serde_json::json!({
            "id": 1, "url": "https://test.example/api/v1/organizations/1", "name": "Acme Test",
            "ui_settings": { "theme": "white" },
            "settings": { "annotation_list_table": { "columns": [
                { "visible": true, "column_type": "schema", "width": 120.0,
                  "schema_id": "field_a", "data_type": "string" }
            ] } }
        }),
    );
    write(
        &root.join("envs/prod/organization.json"),
        &serde_json::json!({
            "id": 2, "url": "https://prod.example/api/v1/organizations/2", "name": "Acme Prod",
            "ui_settings": { "theme": "dark" },
            "settings": { "annotation_list_table": { "columns": [] } }
        }),
    );
    // The target DOES define `field_a`, so this test stays exclusively about
    // settings promotion — it must not also (silently) exercise the missing-
    // schema_id warning, which has its own dedicated test.
    write(
        &root.join("envs/prod/workspaces/main/queues/invoices/schema.json"),
        &serde_json::json!({ "name": "s", "content": [ { "id": "field_a", "category": "datapoint" } ] }),
    );

    let _guard = cwd_lock();
    std::env::set_current_dir(root).unwrap();
    let out = assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "test", "prod"])
        .assert()
        .success();

    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(
        !format!("{stdout}{stderr}").contains("does not exist"),
        "the promoted column's schema_id exists in the target — no warning \
         should fire here: {stdout}{stderr}"
    );

    let tgt = read_json(&root.join("envs/prod/organization.json"));
    assert_eq!(
        tgt["settings"]["annotation_list_table"]["columns"][0]["schema_id"],
        serde_json::json!("field_a"),
        "settings promoted: {tgt}"
    );
    assert_eq!(tgt["id"], serde_json::json!(2), "target identity preserved");
    assert_eq!(tgt["name"], serde_json::json!("Acme Prod"), "target name preserved");
    assert_eq!(
        tgt["ui_settings"]["theme"],
        serde_json::json!("dark"),
        "ui_settings is env-local and must not be promoted"
    );
}

/// No target `organization.json` → skip with a warning, never emit a
/// settings-only file.
#[test]
fn migrate_skips_the_organization_when_the_target_has_none() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/organization.json"),
        &serde_json::json!({ "id": 1, "name": "Acme Test", "settings": {} }),
    );

    let _guard = cwd_lock();
    std::env::set_current_dir(root).unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "test", "prod"])
        .assert()
        .success();

    assert!(
        !root.join("envs/prod/organization.json").exists(),
        "must not create a target org file out of a source-only body"
    );
}

/// `--mirror` must never prune the target's org file, even when the source env
/// has never been pulled.
#[test]
fn migrate_mirror_never_prunes_the_organization() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/prod/organization.json"),
        &serde_json::json!({ "id": 2, "name": "Acme Prod", "settings": {} }),
    );

    let _guard = cwd_lock();
    std::env::set_current_dir(root).unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "test", "prod", "--mirror"])
        .assert()
        .success();

    assert!(
        root.join("envs/prod/organization.json").exists(),
        "a per-env singleton is never a target-only object"
    );
}

/// An `[organization]` overlay entry beats the promoted value.
#[test]
fn migrate_organization_overlay_wins() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/organization.json"),
        &serde_json::json!({ "id": 1, "name": "Acme Test", "settings": {
            "annotation_list_table": { "columns": [
                { "visible": true, "column_type": "schema", "width": 120.0,
                  "schema_id": "field_a", "data_type": "string" }
            ] } } }),
    );
    write(
        &root.join("envs/prod/organization.json"),
        &serde_json::json!({ "id": 2, "name": "Acme Prod", "settings": {} }),
    );
    std::fs::write(
        root.join("envs/prod/overlay.toml"),
        "version = 1\n\n[organization.settings.annotation_list_table]\n\
         columns = [ { visible = true, column_type = \"meta\", width = 80.0, meta_name = \"status\" } ]\n",
    )
    .unwrap();

    let _guard = cwd_lock();
    std::env::set_current_dir(root).unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "test", "prod"])
        .assert()
        .success();

    let tgt = read_json(&root.join("envs/prod/organization.json"));
    assert_eq!(
        tgt["settings"]["annotation_list_table"]["columns"][0]["meta_name"],
        serde_json::json!("status"),
        "overlay must win over the promoted value: {tgt}"
    );
}

/// A promoted `column_type: "schema"` column whose `schema_id` no target
/// schema defines is warned about, never dropped.
#[test]
fn migrate_warns_about_org_columns_absent_from_the_target_schemas() {
    let project = init_two_env_project();
    let root = project.path();
    write(&root.join("envs/test/organization.json"), &serde_json::json!({
        "id": 1, "name": "Acme Test", "settings": { "annotation_list_table": { "columns": [
            { "visible": true, "column_type": "schema", "width": 1.0,
              "schema_id": "not_in_prod", "data_type": "string" },
            { "visible": true, "column_type": "meta", "width": 1.0, "meta_name": "status" }
        ] } }
    }));
    write(&root.join("envs/prod/organization.json"),
          &serde_json::json!({ "id": 2, "name": "Acme Prod", "settings": {} }));
    write(&root.join("envs/prod/workspaces/main/queues/invoices/schema.json"),
          &serde_json::json!({ "name": "s", "content": [ { "id": "in_prod", "category": "datapoint" } ] }));

    let _guard = cwd_lock();
    std::env::set_current_dir(root).unwrap();
    let out = assert_cmd::Command::cargo_bin("rdc").unwrap()
        .current_dir(root).args(["migrate", "test", "prod"]).assert().success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let all = format!("{stdout}{stderr}");
    assert!(all.contains("not_in_prod"), "must name the missing schema_id: {all}");
    assert!(!all.contains("status"), "a meta column has no schema_id to check: {all}");
}
