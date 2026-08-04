# Env naming & Files-tab UX — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development / executing-plans. Steps use `- [ ]`.

**Goal:** (A) name the first environment when creating a project, (B) rename an existing environment, (C) scope the Files tab to the selected environment's snapshot. Builds on the completed multi-env + promote feature.

**Architecture:** One core addition (`GenericMapping::rename_env`, non-behavioral) + bridge changes (`add_project` takes a first-env name via the existing `AddEnvInput`; new `rename_env`) + Flutter (dialog fields, rename-aware edit, Files-tab rootFolder). Spec: `docs/superpowers/specs/2026-08-04-env-naming-and-files-ux-design.md`.

**Tech Stack:** Rust core (`src/mapping.rs`), bridge (`desktop/rust`), FRB 2.12.0, Flutter/Dart.

## Global Constraints
- No CLI behavior change — the only core edit is a new `GenericMapping::rename_env` method. Everything else is under `desktop/`.
- No customer names/identifiers anywhere (code, tests, commit messages); neutral placeholders.
- Env names validated `[A-Za-z0-9_-]+`, non-empty (reuse `valid_env_name` in `desktop/rust/src/api/rdc.rs`).
- Rename is fully local (no remote calls). On-disk format unchanged.
- After Rust `api` changes: `cd desktop && flutter_rust_bridge_codegen generate` (v2.12.0 installed); commit `desktop/lib/src/rust/**` + `desktop/rust/src/frb_generated.rs`.
- `cargo test`/`flutter test` may prune root `Cargo.lock` — `git checkout -- Cargo.lock` before committing.
- Do NOT use bare `git stash` (shared stash stack) or write to any `$HOME`/`~` path.
- Baseline (fresh worktree): `cd desktop && flutter pub get`; `cd desktop/rust && cargo test` green; `cd desktop && flutter analyze` clean, `flutter test` green.

---

### Task 1: Rust — `GenericMapping::rename_env`, `add_project(project_name, AddEnvInput)`, `rename_env`

**Files:**
- Modify: `src/mapping.rs` (core: `rename_env` method + test), `src/lib.rs` (ensure `pub mod mapping` if not already)
- Modify: `desktop/rust/src/api/rdc.rs` (bridge)
- Regenerate: `desktop/lib/src/rust/**`, `desktop/rust/src/frb_generated.rs`

**Interfaces produced (Dart names in parens):**
- core `GenericMapping::rename_env(&mut self, old: &str, new: &str)`
- bridge `add_project(parent: String, project_name: String, first_env: AddEnvInput) -> Result<ProjectSummary>` (`addProject`) — `AddConnectionInput` removed
- bridge `rename_env(folder: String, old: String, new: String) -> Result<ProjectSummary>` (`renameEnv`)

- [ ] **Step 1: Failing core test** — in `src/mapping.rs` tests, add:
```rust
    #[test]
    fn rename_env_rewrites_row_keys_across_kinds() {
        let mut g = GenericMapping::default();
        let mut row = std::collections::BTreeMap::new();
        row.insert("dev".to_string(), "cost-dev".to_string());
        row.insert("prod".to_string(), "cost-prod".to_string());
        g.queues.push(row);
        let mut hrow = std::collections::BTreeMap::new();
        hrow.insert("dev".to_string(), "validator".to_string());
        g.hooks.push(hrow);
        g.rename_env("dev", "sandbox");
        assert_eq!(g.queues[0].get("sandbox"), Some(&"cost-dev".to_string()));
        assert_eq!(g.queues[0].get("dev"), None);
        assert_eq!(g.queues[0].get("prod"), Some(&"cost-prod".to_string()));
        assert_eq!(g.hooks[0].get("sandbox"), Some(&"validator".to_string()));
    }
```
Run `cargo test -p rdc rename_env_rewrites` → FAIL (no method).

- [ ] **Step 2: Implement `rename_env` in core** — add to `impl GenericMapping` in `src/mapping.rs`:
```rust
    /// Rename an environment across every mapping row (each row is
    /// `BTreeMap<env_name, slug>`). No-op for rows that don't reference `old`,
    /// and a no-op overall when `old == new`. Local, non-behavioral: `migrate`
    /// reads the same rows under the new env name afterward.
    pub fn rename_env(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        for kind in Self::KINDS {
            if let Some(rows) = self.kind_rows_mut(kind) {
                for row in rows.iter_mut() {
                    if let Some(slug) = row.remove(old) {
                        row.insert(new.to_string(), slug);
                    }
                }
            }
        }
    }
```
Confirm `src/lib.rs` has `pub mod mapping;` (the bridge needs `rdc::mapping::GenericMapping`); if it's `mod mapping;`, make it `pub`. Run `cargo test -p rdc rename_env_rewrites` → PASS.

- [ ] **Step 3: Failing bridge tests** — append to the `#[cfg(test)] mod tests` in `desktop/rust/src/api/rdc.rs`:
```rust
    #[test]
    fn add_project_uses_the_given_first_env_name() {
        let tmp = tempfile::tempdir().unwrap();
        let p = add_project(
            tmp.path().display().to_string(),
            "Acme".into(),
            AddEnvInput { name: "prod".into(), api_base: "https://p.test/api/v1/".into(), org_id: 7,
                auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None },
        ).unwrap();
        assert_eq!(p.envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["prod"]);
        assert_eq!(p.envs[0].api_base, "https://p.test/api/v1"); // trailing slash trimmed
        let folder = tmp.path().join(&p.id);
        assert!(folder.join("secrets/prod.secrets.json").exists());
        assert!(!folder.join("secrets/main.secrets.json").exists());
    }

    #[test]
    fn add_project_rejects_invalid_first_env_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = add_project(tmp.path().display().to_string(), "Acme".into(),
            AddEnvInput { name: "../x".into(), api_base: "https://p.test/api/v1".into(), org_id: 1,
                auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None }).unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("environment name"));
    }

    #[test]
    fn rename_env_moves_files_toml_and_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        // seed a 2-env project with a mapping.toml referencing `dev`
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(folder.join("envs/dev/queues")).unwrap();
        std::fs::write(folder.join("envs/dev/queues/x.json"), "{}").unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n").unwrap();
        rdc::secrets::write_secrets_file(&folder, "dev", "tok", None).unwrap();
        std::fs::create_dir_all(folder.join(".rdc")).unwrap();
        std::fs::write(folder.join(".rdc/mapping.toml"),
            "version = 2\n[[queues]]\ndev = \"cost-dev\"\nprod = \"cost-prod\"\n").unwrap();

        let p = rename_env(folder.display().to_string(), "dev".into(), "sandbox".into()).unwrap();
        let names: Vec<String> = p.envs.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"sandbox".to_string()) && names.contains(&"prod".to_string()));
        assert!(!names.contains(&"dev".to_string()));
        assert!(folder.join("envs/sandbox/queues/x.json").exists());
        assert!(!folder.join("envs/dev").exists());
        assert!(folder.join("secrets/sandbox.secrets.json").exists());
        let mapping = std::fs::read_to_string(folder.join(".rdc/mapping.toml")).unwrap();
        assert!(mapping.contains("sandbox = \"cost-dev\"") && !mapping.contains("dev = \"cost-dev\""));
    }

    #[test]
    fn rename_env_rejects_collision_and_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n").unwrap();
        assert!(rename_env(folder.display().to_string(), "dev".into(), "prod".into()).is_err()); // collision
        assert!(rename_env(folder.display().to_string(), "dev".into(), "a/b".into()).is_err());  // invalid
        assert!(rename_env(folder.display().to_string(), "nope".into(), "x".into()).is_err());   // unknown old
    }
```
Run `cd desktop/rust && cargo test add_project rename_env` → FAIL (new signature / `rename_env` missing).

- [ ] **Step 4: Implement the bridge** — in `desktop/rust/src/api/rdc.rs`:
  - **Remove** the `pub struct AddConnectionInput { … }` definition (only `add_project` used it).
  - Replace `add_project` with:
```rust
pub fn add_project(parent: String, project_name: String, first_env: AddEnvInput) -> Result<ProjectSummary> {
    let parent = PathBuf::from(parent);
    let env_name = first_env.name.trim().to_string();
    if !valid_env_name(&env_name) {
        return Err(anyhow!("Environment name may only contain letters, digits, - and _ (and can't be empty)."));
    }
    let used: HashSet<String> = discover::scan(&parent).iter().map(|p| p.name().to_string()).collect();
    let slug = rdc::slug::slugify_unique(&project_name, &used);
    let folder = parent.join(&slug);
    std::fs::create_dir_all(&folder).map_err(|e| anyhow!("creating folder: {e}"))?;
    // credentials first (validates + writes secrets) so a bad credential never leaves a config behind
    write_credentials(&folder, &env_name, first_env.auth_kind,
        first_env.token.as_deref(), first_env.username.as_deref(), first_env.password.as_deref())?;
    let api_base = first_env.api_base.trim_end_matches('/').to_string();
    let rdc_toml = format!("[envs.{env_name}]\napi_base = \"{api_base}\"\norg_id = {}\n", first_env.org_id);
    std::fs::write(folder.join("rdc.toml"), rdc_toml).map_err(|e| anyhow!("writing rdc.toml: {e}"))?;
    discover::find(&parent, &slug).as_ref().map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after add"))
}
```
  - Add `rename_env`:
```rust
/// Rename an environment `old` → `new` entirely locally: move every per-env
/// path, rewrite `.rdc/mapping.toml`, and rename the `[envs.<old>]` section.
pub fn rename_env(folder: String, old: String, new: String) -> Result<ProjectSummary> {
    let folder = PathBuf::from(&folder);
    let new = new.trim().to_string();
    if !valid_env_name(&new) {
        return Err(anyhow!("Environment name may only contain letters, digits, - and _."));
    }
    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    if !cfg.envs.contains_key(&old) {
        return Err(anyhow!("This project has no \"{old}\" environment."));
    }
    if new == old {
        return discover::inspect(&folder).as_ref().map(ProjectSummary::from)
            .ok_or_else(|| anyhow!("Project not found"));
    }
    if cfg.envs.contains_key(&new) {
        return Err(anyhow!("An environment named \"{new}\" already exists in this project."));
    }
    let op = rdc::paths::Paths::for_env(&folder, &old);
    let np = rdc::paths::Paths::for_env(&folder, &new);
    // 1. filesystem moves — env_root + secrets are the substantive ones; state/conflicts best-effort.
    if op.env_root().exists() {
        std::fs::rename(op.env_root(), np.env_root())
            .map_err(|e| anyhow!("moving envs/{old} → envs/{new}: {e}"))?;
    }
    if op.secrets_file().exists() {
        if let Some(parent) = np.secrets_file().parent() { let _ = std::fs::create_dir_all(parent); }
        std::fs::rename(op.secrets_file(), np.secrets_file())
            .map_err(|e| anyhow!("moving secrets: {e}"))?;
    }
    for (o, n) in [
        (op.lockfile(), np.lockfile()),
        (op.env_lock(), np.env_lock()),
        (op.base_cache_root(), np.base_cache_root()),
    ] {
        if o.exists() { let _ = std::fs::rename(&o, &n); }
    }
    let old_conflicts = folder.join(".rdc").join("conflicts").join(&old);
    let new_conflicts = folder.join(".rdc").join("conflicts").join(&new);
    if old_conflicts.exists() { let _ = std::fs::rename(&old_conflicts, &new_conflicts); }
    // 2. rewrite mapping.toml
    let mapping_path = folder.join(".rdc").join("mapping.toml");
    if mapping_path.exists() {
        let mut g = rdc::mapping::GenericMapping::load(&mapping_path).map_err(|e| anyhow!("{e:#}"))?;
        g.rename_env(&old, &new);
        g.save(&mapping_path).map_err(|e| anyhow!("{e:#}"))?;
    }
    // 3. rename the rdc.toml section last (authoritative record)
    if let Some(env_cfg) = cfg.envs.remove(&old) { cfg.envs.insert(new.clone(), env_cfg); }
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    discover::inspect(&folder).as_ref().map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after rename_env"))
}
```
  (Verify `rdc::mapping::GenericMapping::{load,save,rename_env}` are reachable — from Step 2's `pub mod mapping`.)

- [ ] **Step 5: Verify Rust** — `cd desktop/rust && cargo test` green; `cargo test -p rdc mapping` green (from the worktree root). Pristine.
- [ ] **Step 6: Regenerate FRB** — `cd desktop && flutter_rust_bridge_codegen generate`; confirm `addProject(parent, projectName, firstEnv)`, `renameEnv(folder, old, new)` in `rdc.dart` and that `AddConnectionInput` is gone.
- [ ] **Step 7: Commit** — `git add src/mapping.rs src/lib.rs desktop/rust/src/api/rdc.rs desktop/lib/src/rust desktop/rust/src/frb_generated.rs && git commit -m "feat: rename_env (core+bridge) + add_project takes a first-env name"`

---

### Task 2: Dart — AppState + dialogs (A first-env field, B rename field + mid-sync guard)

**Files:** Modify `desktop/lib/src/app_state.dart`, `desktop/lib/src/dialogs.dart`; Test `desktop/test/env_naming_test.dart` (new).

**Interfaces:** consumes `addProject(parent, projectName, AddEnvInput)`, `renameEnv(folder, old, new)`, `editProject(folder, env, EditConnectionInput)`, `AddEnvInput`, `EditConnectionInput`.

- [ ] **Step 1: Failing test** (`desktop/test/env_naming_test.dart`): a unit test that a mid-sync guard helper on `AppState` refuses rename — e.g. `AppState.canRenameEnv(folder, env)` returns false when `syncState[envKey(folder, env)] == SyncState.running`, true otherwise. Run → FAIL.

- [ ] **Step 2: Implement AppState**:
  - `addProjectEntry(String projectName, AddEnvInput firstEnv)` — calls `addProject(parent:, projectName:, firstEnv:)`, reloads, `selectProject(newFolder)`. (Replaces the old `addProjectEntry(AddConnectionInput)`.)
  - `bool canRenameEnv(String folder, String env)` → `syncState[envKey(folder, env)] != SyncState.running`.
  - Make `editEnvEntry(ProjectItem item, EnvSummary env, EditConnectionInput input, {String? newEnvName})` rename-aware: if `newEnvName != null && newEnvName != env.name`: guard `canRenameEnv` (throw a clear message if syncing), call `await renameEnv(folder: item.summary.folder, old: env.name, new: newEnvName)`, then use `newEnvName` as the env for the subsequent `editProject`. Then reload + reselect the (possibly renamed) env. Keep the existing external-path follow logic.

- [ ] **Step 3: Dialogs**:
  - New-project dialog (`_AddConnectionDialogState`): add an `_envName` controller + an `ENVIRONMENT NAME` `_Field` (autofocus false, placed right after project NAME, `hint: 'e.g. prod'`). Validate non-empty in `_submit` (else set `_error`). Build `AddEnvInput(name: _envName.text.trim(), apiBase:…, orgId:…, authKind:…, token/…)` and call `widget.state.addProjectEntry(_name.text.trim(), thatInput)`.
  - Edit dialog (`_EditConnectionDialogState`): add an `_envName` controller pre-filled `widget.env.name` + an `ENVIRONMENT NAME` `_Field`. In `_submit`, pass `newEnvName: _envName.text.trim()` to `editEnvEntry(widget.item, widget.env, EditConnectionInput(...), newEnvName: …)`. Surface the mid-sync guard error via the existing `_error` path.

- [ ] **Step 4: Verify** — `cd desktop && dart analyze lib/` clean; `flutter test test/env_naming_test.dart test/app_state_test.dart test/app_state_env_mgmt_test.dart` green. (`home_page.dart` opens the dialogs but their public constructors are unchanged, so `lib/` compiles. Do NOT run whole `flutter test` — `bridge_test.dart` still uses the old `addProject`/`AddConnectionInput`; fixed in Task 4.)

- [ ] **Step 5: Commit** — `git add desktop/lib/src/app_state.dart desktop/lib/src/dialogs.dart desktop/test/env_naming_test.dart && git commit -m "feat(desktop): first-env name field + env rename in the Edit dialog (mid-sync guarded)"`

---

### Task 3: Dart — Files tab scoped to the environment

**Files:** Modify `desktop/lib/src/home_page.dart`; Test `desktop/test/files_env_scope_test.dart` (new).

- [ ] **Step 1: Failing widget test** (`files_env_scope_test.dart`): seed a project with envs, `selectEnv`, render `MdhScaffold(..., activeTab: 'files')`, and assert the Files panel is rooted at `<folder>/envs/<env>` (e.g. by seeding files under `envs/<env>/` and asserting one shows, while a project-root-only file does NOT). Run → FAIL.

- [ ] **Step 2: Implement** — in `_ConnMain`'s Files-tab branch, change `rootFolder: item.summary.folder` to `rootFolder: [item.summary.folder, 'envs', env.name].join(Platform.pathSeparator)`. In `_FilesPanel._readInto` (or its build), when the target dir doesn't exist, set a friendly state so the panel shows "This environment hasn't been synced yet." instead of a raw read error. (`dart:io` `Directory(_dirPath).existsSync()`.)

- [ ] **Step 3: Verify** — `cd desktop && dart analyze lib/` clean; `flutter test test/files_env_scope_test.dart` green.

- [ ] **Step 4: Commit** — `git add desktop/lib/src/home_page.dart desktop/test/files_env_scope_test.dart && git commit -m "feat(desktop): Files tab nests into envs/<env> with a not-synced empty state"`

---

### Task 4: Goldens + integration tests + whole-suite green

**Files:** Modify `desktop/test/golden_mdh_test.dart` (Files fixture), regenerate `desktop/test/goldens/mdh_files*.png`; Modify `desktop/integration_test/bridge_test.dart`.

- [ ] **Step 1: Files golden fixture** — in `golden_mdh_test.dart`, update `_filesFixture()`/`_filesState()` so the previewable files live UNDER `envs/main/`: e.g. a JSON at `envs/main/schemas/invoices.json` (for the JSON-preview golden) and a TOML at `envs/main/hooks/config.toml` (for the TOML-preview golden). Update the two preview tests to tap those env-tree files (`find.text('invoices.json')` / `find.text('config.toml')`). The list-view golden now shows `envs/main`'s contents (hooks/, queues/, schemas/).

- [ ] **Step 2: Regenerate** — `cd desktop && flutter test --update-goldens test/golden_mdh_test.dart`; run without the flag to confirm green. Visually confirm `mdh_files_light.png` shows the env's contents (not the project root) and the two preview goldens render the TOML/JSON.

- [ ] **Step 3: Integration test** — in `desktop/integration_test/bridge_test.dart`: update every `addProject(parent:, input: AddConnectionInput(...))` call to `addProject(parent:, projectName: '<name>', firstEnv: AddEnvInput(name: '<env>', apiBase:…, orgId:…, authKind:…, token:…))`; adjust assertions to the chosen first-env name (no longer `main`). Add a rename round-trip: create a project + `addEnv('prod')`, `renameEnv(folder, 'prod', 'staging')`, assert `listProjects` shows the renamed env, `secrets/staging.secrets.json` exists, `secrets/prod.secrets.json` does not.

- [ ] **Step 4: Whole suite** — `cd desktop && flutter analyze` clean; `flutter test` all green; `cd desktop/rust && cargo test` green; `cargo test -p rdc mapping` green; attempt `flutter test integration_test -d macos` and report. `git checkout -- Cargo.lock` if pruned.

- [ ] **Step 5: Commit** — `git add desktop/test desktop/integration_test && git commit -m "test(desktop): env-scoped Files goldens + add_project/rename_env integration coverage"`

---

## Self-review notes
- Spec coverage: §5 A → Task 1 (add_project) + Task 2 (dialog) + Task 4 (integration); §6 B → Task 1 (core+bridge rename_env) + Task 2 (Edit-dialog field, guard) + Task 4 (integration); §7 C → Task 3 + Task 4 (goldens).
- Coupling (like the prior feature): Task 1 changes `add_project`'s signature + removes `AddConnectionInput`, so Dart doesn't fully compile until Tasks 2 & 4. Per-task gates use `dart analyze lib/` + targeted tests; Task 4 restores whole-suite green (it fixes `bridge_test.dart`, the last `AddConnectionInput` user).
- Type consistency: Rust `add_project(parent, project_name, first_env: AddEnvInput)` ↔ Dart `addProject(parent:, projectName:, firstEnv:)`; `rename_env(folder, old, new)` ↔ `renameEnv(folder:, old:, new:)`; `editEnvEntry(item, env, input, {newEnvName})` orchestrates rename-then-edit.
- Risk (from spec §9): env rename non-atomicity — validate-first + small final `rdc.toml` write + mid-sync guard; documented, accepted.
