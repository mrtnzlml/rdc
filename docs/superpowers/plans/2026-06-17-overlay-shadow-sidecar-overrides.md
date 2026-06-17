# Shadow-file Sidecar Overrides Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `rdc migrate <src> <tgt>` override the content of code/formula sidecar files per target env via shadow files under `envs/<env>/overlay/`, with a shadow that overwrites no source sidecar being a hard error.

**Architecture:** A new per-env `overlay/` directory mirrors the snapshot tree. During `migrate`, the non-JSON (sidecar) branch of `transform_file` writes `overlay/<target-relpath>` instead of the verbatim source bytes when such a shadow exists. Before the write loop, `run` validates that every file under `overlay/` corresponds to a sidecar the migration produces (full source set, independent of `--only`); otherwise it aborts. `overlay.toml`, the `Overlay` struct, hashing, and pull/sync are untouched. Spec: `docs/superpowers/specs/2026-06-17-overlay-shadow-sidecar-overrides-design.md`.

**Tech Stack:** Rust, `anyhow`, `serde_json`, existing `assert_cmd`-based integration harness in `tests/cli_migrate.rs`. No new dependency.

---

## File Structure

- `src/paths.rs` — add `OVERLAY_DIR` const + `Paths::overlay_dir()` (the `envs/<env>/overlay/` path). One source of truth for the directory name.
- `src/cli/migrate/mod.rs` — add three private helpers (`is_sidecar`, `list_overlay_files`, `validate_overlay_dir`); apply the shadow in `transform_file`'s non-JSON branch (no signature change — derived from `tgt_root`); wire validation + the produced-sidecar set into `run`. Unit tests go in the existing `#[cfg(test)] mod tests` (starts at line 767).
- `tests/cli_migrate.rs` — end-to-end coverage via `rdc::cli::migrate::run`.

Key verified facts the plan relies on:
- `transform_file` non-JSON branch copies verbatim (`src/cli/migrate/mod.rs:232-237`); it already has `tgt_root`, and `dst = tgt_root.join(remap_relative(rel, mapping))`.
- `classify_for_selection` maps a sidecar `.py`/`.js` to its `(kind, slug)` (`:117-145`); returns `None` for non-sidecars.
- `enumerate_files(env_root, env)` returns the full managed-file set (`:413-423`); `run` applies `--only` only inside the loop (`:568-573`).
- `write_atomic(&Path, &[u8])` (`src/snapshot/writer.rs:12`).
- `transform_file` callers (must NOT break): `run` (`:587`) + unit tests at `:939, :985, :1027, :1075, :1116, :1181, :1237` — all keep working because the signature is unchanged.

---

### Task 1: `Paths::overlay_dir()` path helper

**Files:**
- Modify: `src/paths.rs` (add const near top of `impl Paths`, method beside `overlay_file()` at `:80-83`)
- Test: `src/paths.rs` (existing `#[cfg(test)] mod tests`, beside `overlay_file_path` at `:280`)

- [ ] **Step 1: Write the failing test**

Add inside the `mod tests` block in `src/paths.rs` (the `p()` helper there returns `Paths` for env `dev` rooted at `/proj`):

```rust
    #[test]
    fn overlay_dir_path() {
        assert_eq!(p().overlay_dir(), Path::new("/proj/envs/dev/overlay"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib paths::tests::overlay_dir_path`
Expected: FAIL — `no method named overlay_dir found`.

- [ ] **Step 3: Add the const and method**

In `src/paths.rs`, add the const just above the `impl Paths` block (or with the other associated items):

```rust
/// Name of the per-env shadow-override directory (`envs/<env>/overlay/`),
/// sibling of `overlay.toml`. Files under it shadow code/formula sidecars
/// during `rdc migrate`.
pub(crate) const OVERLAY_DIR: &str = "overlay";
```

Add the method right after `overlay_file()` (after `:83`):

```rust
    /// `<root>/envs/<env>/overlay/` — shadow-override directory. Files mirror
    /// the snapshot tree and replace code/formula sidecars during `migrate`.
    pub fn overlay_dir(&self) -> PathBuf {
        self.env_root().join(OVERLAY_DIR)
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib paths::tests::overlay_dir_path`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/paths.rs
git commit -m "feat(paths): add overlay_dir() for the per-env shadow directory

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `is_sidecar` helper

**Files:**
- Modify: `src/cli/migrate/mod.rs` (add helper after `classify_for_selection`, ~`:145`)
- Test: `src/cli/migrate/mod.rs` (`mod tests`, `:767`)

- [ ] **Step 1: Write the failing test**

Add inside `mod tests` in `src/cli/migrate/mod.rs`:

```rust
    #[test]
    fn is_sidecar_matches_only_code_files() {
        // Code/formula sidecars → true.
        assert!(is_sidecar(Path::new("hooks/extractor.py")));
        assert!(is_sidecar(Path::new("hooks/extractor.js")));
        assert!(is_sidecar(Path::new("rules/r1.py")));
        assert!(is_sidecar(Path::new(
            "workspaces/main/queues/invoices/formulas/sftp_path.py"
        )));
        // JSON objects → false (overlay.toml handles those).
        assert!(!is_sidecar(Path::new("hooks/extractor.json")));
        assert!(!is_sidecar(Path::new(
            "workspaces/main/queues/invoices/schema.json"
        )));
        // Non-sidecar code → false.
        assert!(!is_sidecar(Path::new("workspaces/main/workspace.py")));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli::migrate::tests::is_sidecar_matches_only_code_files`
Expected: FAIL — `cannot find function is_sidecar`.

- [ ] **Step 3: Implement the helper**

Add after `classify_for_selection` (after `:145`) in `src/cli/migrate/mod.rs`:

```rust
/// True for a non-JSON code/formula sidecar leaf (`.py`/`.js`) that belongs to
/// a hook, rule, or schema — the files `migrate` copies verbatim and that an
/// `overlay/` shadow may replace. JSON objects are excluded (they are
/// overlay-able through `overlay.toml`); non-sidecar code returns false.
fn is_sidecar(rel: &Path) -> bool {
    let is_json = rel
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    !is_json && classify_for_selection(rel).is_some()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib cli::migrate::tests::is_sidecar_matches_only_code_files`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/migrate/mod.rs
git commit -m "feat(migrate): add is_sidecar helper for shadow-override scope

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: `list_overlay_files` helper

**Files:**
- Modify: `src/cli/migrate/mod.rs` (add helper near `validate_overlay_dir`, e.g. after `is_sidecar`)
- Test: `src/cli/migrate/mod.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn list_overlay_files_returns_relpaths_skipping_pycache() {
        use std::fs;
        let dir = tempfile::TempDir::new().unwrap();
        let ov = dir.path();
        fs::create_dir_all(ov.join("hooks")).unwrap();
        fs::write(ov.join("hooks/extractor.py"), b"x").unwrap();
        fs::create_dir_all(ov.join("workspaces/main/queues/invoices/formulas")).unwrap();
        fs::write(
            ov.join("workspaces/main/queues/invoices/formulas/sftp_path.py"),
            b"y",
        )
        .unwrap();
        // __pycache__ must be ignored.
        fs::create_dir_all(ov.join("hooks/__pycache__")).unwrap();
        fs::write(ov.join("hooks/__pycache__/extractor.cpython-312.pyc"), b"z").unwrap();

        let got: Vec<std::path::PathBuf> = list_overlay_files(ov).unwrap();
        assert_eq!(
            got,
            vec![
                std::path::PathBuf::from("hooks/extractor.py"),
                std::path::PathBuf::from(
                    "workspaces/main/queues/invoices/formulas/sftp_path.py"
                ),
            ]
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli::migrate::tests::list_overlay_files_returns_relpaths_skipping_pycache`
Expected: FAIL — `cannot find function list_overlay_files`.

- [ ] **Step 3: Implement the helper**

Add in `src/cli/migrate/mod.rs` (near `is_sidecar`):

```rust
/// Recursively list files under `overlay_dir`, returning paths RELATIVE to it,
/// sorted. Skips `__pycache__` directories (tooling output), mirroring
/// [`walk_dir`]. Returns an empty vec if `overlay_dir` does not exist.
fn list_overlay_files(overlay_dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if entry.file_name() == "__pycache__" {
                    continue;
                }
                walk(base, &path, out)?;
            } else {
                out.push(
                    path.strip_prefix(base)
                        .expect("walked path is under base")
                        .to_path_buf(),
                );
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if overlay_dir.exists() {
        walk(overlay_dir, overlay_dir, &mut out)?;
    }
    out.sort();
    Ok(out)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib cli::migrate::tests::list_overlay_files_returns_relpaths_skipping_pycache`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/migrate/mod.rs
git commit -m "feat(migrate): add list_overlay_files helper

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: `validate_overlay_dir`

**Files:**
- Modify: `src/cli/migrate/mod.rs` (add after `list_overlay_files`)
- Test: `src/cli/migrate/mod.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn validate_overlay_dir_ok_when_all_files_match_produced() {
        use std::fs;
        let dir = tempfile::TempDir::new().unwrap();
        let ov = dir.path();
        fs::create_dir_all(ov.join("hooks")).unwrap();
        fs::write(ov.join("hooks/extractor.py"), b"x").unwrap();

        let produced: std::collections::BTreeSet<PathBuf> =
            [PathBuf::from("hooks/extractor.py")].into_iter().collect();
        assert!(validate_overlay_dir(ov, &produced).is_ok());
    }

    #[test]
    fn validate_overlay_dir_errors_on_dangling_shadow() {
        use std::fs;
        let dir = tempfile::TempDir::new().unwrap();
        let ov = dir.path();
        fs::create_dir_all(ov.join("hooks")).unwrap();
        fs::write(ov.join("hooks/ghost.py"), b"x").unwrap();

        let produced: std::collections::BTreeSet<PathBuf> =
            [PathBuf::from("hooks/extractor.py")].into_iter().collect();
        let err = validate_overlay_dir(ov, &produced).unwrap_err().to_string();
        assert!(err.contains("hooks/ghost.py"), "names the offending file: {err}");
    }

    #[test]
    fn validate_overlay_dir_errors_on_json_shadow() {
        use std::fs;
        let dir = tempfile::TempDir::new().unwrap();
        let ov = dir.path();
        fs::create_dir_all(ov.join("hooks")).unwrap();
        fs::write(ov.join("hooks/extractor.json"), b"{}").unwrap();

        // `produced` is sidecars only — JSON is never in it.
        let produced: std::collections::BTreeSet<PathBuf> =
            [PathBuf::from("hooks/extractor.py")].into_iter().collect();
        let err = validate_overlay_dir(ov, &produced).unwrap_err().to_string();
        assert!(err.contains("hooks/extractor.json"), "names the json file: {err}");
    }

    #[test]
    fn validate_overlay_dir_ok_when_dir_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let produced = std::collections::BTreeSet::new();
        assert!(validate_overlay_dir(&dir.path().join("overlay"), &produced).is_ok());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cli::migrate::tests::validate_overlay_dir`
Expected: FAIL — `cannot find function validate_overlay_dir`.

- [ ] **Step 3: Implement `validate_overlay_dir`**

Add in `src/cli/migrate/mod.rs` after `list_overlay_files`:

```rust
/// Validate the target env's `overlay/` shadow directory. Every file under it
/// must mirror a code/formula sidecar that migrating the source produces in the
/// target — i.e. its relpath must be in `produced` (the full source enumeration
/// remapped to target paths, filtered to sidecars, independent of `--only`). A
/// shadow that overwrites nothing — a typo, a stale path, a `.json`, or a
/// sidecar absent from the source — is a hard error naming the offending files.
/// Run BEFORE any target file is written so the migration aborts cleanly.
fn validate_overlay_dir(
    overlay_dir: &Path,
    produced: &std::collections::BTreeSet<PathBuf>,
) -> Result<()> {
    let offenders: Vec<PathBuf> = list_overlay_files(overlay_dir)?
        .into_iter()
        .filter(|rel| !produced.contains(rel))
        .collect();
    if !offenders.is_empty() {
        let list = offenders
            .iter()
            .map(|p| format!("  - overlay/{}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "overlay/ contains shadow file(s) that overwrite no source code/formula sidecar:\n\
             {list}\n\
             Each file under envs/<env>/overlay/ must mirror a sidecar produced by migrating the \
             source (a hook/rule .py/.js, or a queue's formulas/<field>.py). Fix the path or remove it."
        );
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib cli::migrate::tests::validate_overlay_dir`
Expected: PASS (all four).

- [ ] **Step 5: Commit**

```bash
git add src/cli/migrate/mod.rs
git commit -m "feat(migrate): validate overlay/ shadow dir against produced sidecars

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Apply the shadow in `transform_file` and wire validation into `run`

**Files:**
- Modify: `src/cli/migrate/mod.rs` — `transform_file` (`:222-237`) and `run` (`:560` area, before the loop)
- Test: `tests/cli_migrate.rs` (new integration test)

- [ ] **Step 1: Write the failing integration test**

Add to `tests/cli_migrate.rs` (uses the existing `init_two_env_project`, `write`, `cwd_lock` helpers; identity slugs need no mapping file — `auto_match` yields an empty map and `tgt_slug` falls back to identity):

```rust
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![]);
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test cli_migrate migrate_overlay_shadow_replaces_formula_sidecar`
Expected: FAIL — assertion: prod formula still holds `"/Test/path"` (verbatim copy).

- [ ] **Step 3: Apply the shadow in `transform_file`**

In `src/cli/migrate/mod.rs`, replace the start of `transform_file` (currently `:223-237`):

```rust
    let src_path = src_root.join(rel);
    let dst_path = tgt_root.join(remap_relative(rel, mapping));

    let is_json = rel
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if !is_json {
        let bytes =
            std::fs::read(&src_path).with_context(|| format!("reading {}", src_path.display()))?;
        crate::snapshot::writer::write_atomic(&dst_path, &bytes)?;
        return Ok(());
    }
```

with (note `dst_rel` is now bound separately so the shadow path can reuse it):

```rust
    let src_path = src_root.join(rel);
    let dst_rel = remap_relative(rel, mapping);
    let dst_path = tgt_root.join(&dst_rel);

    let is_json = rel
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if !is_json {
        // Shadow override: a file at <env>/overlay/<dst_rel> replaces the source
        // sidecar's content for this target env. `run` validates the overlay dir
        // up-front, so any shadow present here mirrors a real source sidecar.
        let shadow = tgt_root.join(crate::paths::OVERLAY_DIR).join(&dst_rel);
        let bytes = if shadow.is_file() {
            std::fs::read(&shadow)
                .with_context(|| format!("reading overlay shadow {}", shadow.display()))?
        } else {
            std::fs::read(&src_path).with_context(|| format!("reading {}", src_path.display()))?
        };
        crate::snapshot::writer::write_atomic(&dst_path, &bytes)?;
        return Ok(());
    }
```

- [ ] **Step 4: Wire validation + produced-sidecar set into `run`**

In `src/cli/migrate/mod.rs`, immediately after `let files = enumerate_files(&src_root, src)?;` (`:560`) and before `let mut copied = 0usize;`, insert:

```rust
    // Validate the target env's `overlay/` shadow dir before writing anything:
    // every shadow must mirror a sidecar this migration produces. Built from the
    // FULL source enumeration (not the `--only` subset), so scoping with `--only`
    // never falsely flags a valid shadow it simply did not apply this run.
    let produced_sidecars: std::collections::BTreeSet<PathBuf> = files
        .iter()
        .filter(|rel| is_sidecar(rel))
        .map(|rel| remap_relative(rel, &mapping))
        .collect();
    validate_overlay_dir(&tgt_paths.overlay_dir(), &produced_sidecars)?;
```

- [ ] **Step 5: Run the integration test + the full migrate suites to verify pass + no regressions**

Run: `cargo test --test cli_migrate migrate_overlay_shadow_replaces_formula_sidecar`
Expected: PASS.

Run: `cargo test --lib cli::migrate:: && cargo test --test cli_migrate`
Expected: PASS (all existing migrate unit + integration tests still green — `transform_file`'s signature is unchanged, and tgt tempdirs have no `overlay/` so the verbatim path is preserved).

- [ ] **Step 6: Commit**

```bash
git add src/cli/migrate/mod.rs tests/cli_migrate.rs
git commit -m "feat(migrate): apply overlay/ shadow files to sidecars; validate up-front

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: End-to-end coverage (hook/rule/untouched/errors/--only)

**Files:**
- Test: `tests/cli_migrate.rs` (add the tests below)

These exercise the full mechanism implemented in Task 5; they should PASS on first run (they confirm wiring + error paths end-to-end).

- [ ] **Step 1: Add the integration tests**

```rust
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![]);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![]);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![]);
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
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec!["hooks/a".to_string()]);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("a valid shadow for an --only-excluded sidecar must not error");

    let prod = root.join("envs/prod");
    assert!(prod.join("hooks/a.json").exists(), "selected object migrated");
    assert!(!prod.join("hooks/b.json").exists(), "excluded object not migrated");
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test --test cli_migrate migrate_overlay_`
Expected: PASS (all overlay integration tests).

- [ ] **Step 3: Commit**

```bash
git add tests/cli_migrate.rs
git commit -m "test(migrate): end-to-end coverage for overlay/ shadow overrides

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Documentation

**Files:**
- Modify: `src/cli/migrate/mod.rs` module doc-comment (`:1-15`)
- Modify: `src/cli/mod.rs` — the `Migrate` variant doc-comment (`:171-202`, shown in `rdc migrate --help`)

- [ ] **Step 1: Update the migrate module doc-comment**

In `src/cli/migrate/mod.rs`, extend the numbered list in the header (after step 4, `:12`) with:

```rust
//! 5. replace a code/formula sidecar's content when the target env carries a
//!    shadow file at `envs/<tgt>/overlay/<same-relpath>` (whole-file override;
//!    a shadow that mirrors no source sidecar is a hard error).
```

- [ ] **Step 2: Update the `Migrate` CLI help**

In `src/cli/mod.rs`, append to the `Migrate` doc-comment (before the `Migrate {` struct at `:180`) a paragraph:

```rust
    /// Per-env code overrides: a file at `envs/<tgt>/overlay/<relpath>` replaces
    /// the migrated code/formula sidecar at `<relpath>` (hook/rule `.py`/`.js` or
    /// a queue's `formulas/<field>.py`). A shadow that overrides no source
    /// sidecar aborts the migration. Requires rdc >= the release that ships this.
```

- [ ] **Step 3: Verify it builds and help renders**

Run: `cargo build && cargo run -- migrate --help`
Expected: build succeeds; the help text includes the overlay override paragraph.

- [ ] **Step 4: Commit**

```bash
git add src/cli/migrate/mod.rs src/cli/mod.rs
git commit -m "docs(migrate): document overlay/ shadow sidecar overrides

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Final verification

- [ ] **Step 1: Full test suite + lints**

Run: `cargo test`
Expected: PASS (whole workspace).

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings (the codebase treats clippy warnings as errors — see workspace lints).

Run: `cargo fmt --check`
Expected: clean.

- [ ] **Step 2: Manual smoke test (optional, real project)**

In a scratch project, create `envs/<tgt>/overlay/.../formulas/<field>.py`, run `rdc migrate <src> <tgt>`, and confirm the target sidecar holds the shadow content and `rdc sync <tgt> --dry-run` shows it as ordinary content.

---

## Self-Review

**Spec coverage:**
- `overlay/` dir convention → Task 1 (path), Task 5 (apply). ✓
- Whole-file shadow replacement during migrate → Task 5. ✓
- Sidecars-only scope guard → Task 2 (`is_sidecar`), enforced via `produced` in Task 4/5; `.json` rejected (Task 4 + Task 6 e2e). ✓
- Transparent-error rule (dangling = hard error, fail-fast) → Task 4 (unit), Task 5 (wired before loop), Task 6 (e2e + "writes nothing"). ✓
- `--only` uses full produced set → Task 5 (built from full `files`), Task 6 (`migrate_overlay_only_excluded_shadow_does_not_error`). ✓
- `--mirror`/hashing/pull-sync untouched → no code in those paths changed; `overlay/` not in `MANAGED_DIRS`. ✓ (No task needed; called out here so the reviewer confirms nothing else is touched.)
- Backward compat (overlay.toml/`Overlay` struct unchanged, additive) → no struct/TOML changes in any task. ✓
- Docs / "requires rdc >=" note → Task 7. ✓
- Testing matrix (formula, hook, rule, untouched, dangling, json, --only, sync parity) → Tasks 5-6; sync-parity is covered by Task 8 Step 2 smoke test (migrate writes ordinary sidecar content, no hashing change).

**Placeholder scan:** No TBD/TODO; every code step shows full code; every command has expected output. ✓

**Type consistency:** `is_sidecar(&Path) -> bool`, `list_overlay_files(&Path) -> Result<Vec<PathBuf>>`, `validate_overlay_dir(&Path, &BTreeSet<PathBuf>) -> Result<()>`, `Paths::overlay_dir() -> PathBuf`, const `paths::OVERLAY_DIR: &str`. `transform_file` signature unchanged; shadow path = `tgt_root.join(paths::OVERLAY_DIR).join(&dst_rel)`. Names consistent across Tasks 1-7. ✓
