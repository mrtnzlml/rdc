# Desktop multi-env — Phase 2 (env management + Project view) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Let a project hold several environments *managed from the app* — add / remove an environment — and make the project's sidebar node a real destination: a **Project view** that lists all environments (the evolution of the old single-`main` view). Builds on Phase 1.

**Architecture:** Bridge gains `add_env` / `remove_env` (the `rdc` core is untouched; they edit `rdc.toml` + per-env secrets/snapshot via the same on-disk contract). `AppState` gains `addEnvEntry` / `removeEnvEntry` and `selectProject` now shows the Project view (`selectedEnv == null`) instead of auto-selecting the first env. A new `_ProjectView` renders the Environments table with per-env Sync/Edit/Remove + an "Add environment" action. Promote is NOT added here — that is Phase 3, which drops a Promote panel into this same Project view.

**Tech Stack:** Rust bridge (`desktop/rust`), FRB 2.12.0 (regenerate + commit), Flutter/Dart.

## Global Constraints
- CLI core (`rdc`) is NOT modified — only `desktop/`. No customer names/identifiers anywhere (code, tests, commit messages); neutral placeholders only.
- On-disk format unchanged: `rdc.toml [envs.<name>]`, `envs/<name>/`, `secrets/<name>.secrets.json`, `.rdc/state/<name>.lock.json`.
- Env-level sync stays pull-only. No remote-write path in this phase.
- After Rust `api` changes: `cd desktop && flutter_rust_bridge_codegen generate` (v2.12.0 installed); commit `desktop/lib/src/rust/**` + `desktop/rust/src/frb_generated.rs`.
- `add_project` still creates a first env named `main` (unchanged from Phase 1); free-form first-env naming remains deferred. Additional envs are created via `add_env`.
- Baseline before starting: `cd desktop/rust && cargo test` green; `cd desktop && flutter analyze` clean; `flutter test` green.

---

### Task 1: Bridge `add_env` / `remove_env`

**Files:**
- Modify: `desktop/rust/src/api/rdc.rs`
- Regenerate: `desktop/lib/src/rust/**`, `desktop/rust/src/frb_generated.rs`

**Interfaces:**
- Consumes: `discover::inspect`, `ProjectSummary`, `AuthKind`, `write_credentials(folder, env, …)` (Phase 1), `rdc::config::{ProjectConfig, EnvConfig}`, `trash_project`.
- Produces (Dart names in parens):
  - `struct AddEnvInput { name: String, api_base: String, org_id: u64, auth_kind: AuthKind, token: Option<String>, username: Option<String>, password: Option<String> }`
  - `fn add_env(folder: String, input: AddEnvInput) -> Result<ProjectSummary>` (`addEnv`)
  - `fn remove_env(folder: String, env: String) -> Result<Option<ProjectSummary>>` (`removeEnv`) — `None` when the removed env was the last one (whole project trashed)

- [ ] **Step 1: Write failing Rust tests** — append to the `#[cfg(test)] mod tests` in `desktop/rust/src/api/rdc.rs`:

```rust
    fn seed_project(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.main]\napi_base = \"https://m.test/api/v1\"\norg_id = 1\n").unwrap();
        rdc::secrets::write_secrets_file(&folder, "main", "tok", None).unwrap();
        folder
    }

    #[test]
    fn add_env_appends_a_second_env() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let p = add_env(folder.display().to_string(), AddEnvInput {
            name: "prod".into(), api_base: "https://p.test/api/v1/".into(), org_id: 2,
            auth_kind: AuthKind::Token, token: Some("tok2".into()), username: None, password: None,
        }).unwrap();
        assert_eq!(p.envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["main", "prod"]);
        let prod = p.envs.iter().find(|e| e.name == "prod").unwrap();
        assert_eq!(prod.api_base, "https://p.test/api/v1"); // trailing slash trimmed
        assert!(folder.join("secrets/prod.secrets.json").exists());
    }

    #[test]
    fn add_env_rejects_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let err = add_env(folder.display().to_string(), AddEnvInput {
            name: "main".into(), api_base: "https://x.test/api/v1".into(), org_id: 9,
            auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None,
        }).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"));
    }

    #[test]
    fn remove_env_drops_one_and_keeps_project() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        add_env(folder.display().to_string(), AddEnvInput {
            name: "prod".into(), api_base: "https://p.test/api/v1".into(), org_id: 2,
            auth_kind: AuthKind::Token, token: Some("t2".into()), username: None, password: None,
        }).unwrap();
        let p = remove_env(folder.display().to_string(), "prod".into()).unwrap().unwrap();
        assert_eq!(p.envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["main"]);
        assert!(!folder.join("secrets/prod.secrets.json").exists());
    }

    #[test]
    fn remove_last_env_trashes_project_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let r = remove_env(folder.display().to_string(), "main".into()).unwrap();
        assert!(r.is_none());
        assert!(!folder.exists()); // whole project gone (moved to trash)
    }
```

- [ ] **Step 2: Run to verify failure** — `cd desktop/rust && cargo test add_env remove_env` → FAIL to compile (`AddEnvInput`/`add_env`/`remove_env` missing).

- [ ] **Step 3: Implement** — add to `desktop/rust/src/api/rdc.rs` (near `edit_project`):

```rust
#[derive(Debug, Clone)]
pub struct AddEnvInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Add a new environment to an existing project. Errors if the env already exists.
pub fn add_env(folder: String, input: AddEnvInput) -> Result<ProjectSummary> {
    let folder = PathBuf::from(&folder);
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(anyhow!("Environment name is required."));
    }
    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    if cfg.envs.contains_key(&name) {
        return Err(anyhow!("An environment named \"{name}\" already exists in this project."));
    }
    cfg.envs.insert(
        name.clone(),
        rdc::config::EnvConfig {
            api_base: input.api_base.trim_end_matches('/').to_string(),
            org_id: input.org_id,
        },
    );
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    write_credentials(
        &folder, &name,
        input.auth_kind, input.token.as_deref(), input.username.as_deref(), input.password.as_deref(),
    )?;
    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after add_env"))
}

/// Remove an environment: its `rdc.toml` section, snapshot, secrets, and state.
/// If it was the last env, the whole project is trashed and `Ok(None)` returned.
pub fn remove_env(folder: String, env: String) -> Result<Option<ProjectSummary>> {
    let folder = PathBuf::from(&folder);
    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    if !cfg.envs.contains_key(&env) {
        return Err(anyhow!("This project has no \"{env}\" environment."));
    }
    if cfg.envs.len() == 1 {
        // Last environment — removing it means removing the project.
        trash_project(folder.display().to_string())?;
        return Ok(None);
    }
    cfg.envs.remove(&env);
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    // Best-effort cleanup of the env's on-disk artifacts.
    let _ = std::fs::remove_dir_all(folder.join(format!("envs/{env}")));
    let _ = std::fs::remove_file(folder.join(format!("secrets/{env}.secrets.json")));
    let _ = std::fs::remove_file(folder.join(format!(".rdc/state/{env}.lock.json")));
    let _ = std::fs::remove_file(folder.join(format!(".rdc/state/{env}.lock")));
    let _ = std::fs::remove_dir_all(folder.join(format!(".rdc/state/{env}.base")));
    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .map(Some)
        .ok_or_else(|| anyhow!("Project not found after remove_env"))
}
```

- [ ] **Step 4: Verify** — `cd desktop/rust && cargo test` → all green, pristine.
- [ ] **Step 5: Regenerate bindings** — `cd desktop && flutter_rust_bridge_codegen generate`; confirm `addEnv`/`removeEnv`/`AddEnvInput` appear in `desktop/lib/src/rust/api/rdc.dart`.
- [ ] **Step 6: Commit** — `git add desktop/rust/src/api/rdc.rs desktop/lib/src/rust desktop/rust/src/frb_generated.rs && git commit -m "feat(desktop): bridge add_env / remove_env"`

---

### Task 2: `AppState` env management + selection model + dialogs

**Files:**
- Modify: `desktop/lib/src/app_state.dart`, `desktop/lib/src/dialogs.dart`
- Test: `desktop/test/app_state_env_mgmt_test.dart` (new)

**Interfaces:**
- Consumes: `addEnv`, `removeEnv`, `AddEnvInput` (Task 1); existing `ProjectItem`, `EditConnectionInput`.
- Produces (for Task 3):
  - `AppState.addEnvEntry(ProjectItem item, AddEnvInput input)` — calls `addEnv`, reloads, selects the new env.
  - `AppState.removeEnvEntry(ProjectItem item, EnvSummary env)` — calls `removeEnv`; on `null` (project gone) clears selection; else re-selects the project (Project view). Reloads.
  - `selectProject(folder)` now sets `selectedEnv = null` (→ Project view), NOT the first env.
  - Dialogs: `AddEnvDialog(state, item)` (env name + connection fields → `addEnvEntry`); `RemoveEnvDialog(env)` (confirm). `EditConnectionDialog` unchanged (already env-scoped).

- [ ] **Step 1: Write the failing test** — `desktop/test/app_state_env_mgmt_test.dart`:

```dart
import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter_test/flutter_test.dart';

ProjectItem _p(String folder, List<String> envs) => ProjectItem(
      ProjectSummary(id: folder, name: folder.split('/').last, folder: folder,
          envs: [for (final n in envs) EnvSummary(name: n, apiBase: 'https://x.test/api/v1',
              orgId: BigInt.one, authKind: AuthKind.token, lastSyncUnix: null, fileCount: BigInt.zero)]),
      false);

void main() {
  test('selectProject shows the Project view (no env pinned)', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    s.projects = [_p('/tmp/acme', ['main', 'prod'])];
    s.selectProject('/tmp/acme');
    expect(s.selectedFolder, '/tmp/acme');
    expect(s.selectedEnv, isNull);           // Project view
    expect(s.selectedEnvSummary, isNull);
    s.selectEnv('/tmp/acme', 'prod');
    expect(s.selectedEnv, 'prod');           // env view
  });
}
```

- [ ] **Step 2: Verify failure** — `cd desktop && flutter test test/app_state_env_mgmt_test.dart` → FAIL (`selectProject` still pins first env).

- [ ] **Step 3: Implement AppState** — in `desktop/lib/src/app_state.dart`:
  - Change `selectProject` so it does NOT pin an env:
    ```dart
    void selectProject(String folder) {
      selectedFolder = folder;
      selectedEnv = null; // show the Project view; env children are selected explicitly
      notifyListeners();
    }
    ```
  - Add:
    ```dart
    Future<void> addEnvEntry(ProjectItem item, AddEnvInput input) async {
      await addEnv(folder: item.summary.folder, input: input);
      await reload();
      selectEnv(item.summary.folder, input.name.trim());
    }

    Future<void> removeEnvEntry(ProjectItem item, EnvSummary env) async {
      final updated = await removeEnv(folder: item.summary.folder, env: env.name);
      await reload();
      if (updated == null) {
        // whole project was removed (last env)
        if (selectedFolder == item.summary.folder) { selectedFolder = null; selectedEnv = null; }
      } else {
        selectProject(item.summary.folder); // back to the Project view
      }
    }
    ```
  - NOTE: `reload()`'s existing fallback (`if (selectedFolder == null && projects.isNotEmpty) selectProject(first)`) now lands on the Project view — correct. The "re-pin vanished env" block (lines ~115-118) only runs when `selectedEnv != null`, so it's unaffected when a project is selected.

- [ ] **Step 4: Add dialogs** — in `desktop/lib/src/dialogs.dart`, add `AddEnvDialog` (model it on `AddConnectionDialog` but with an ENV NAME field first, title `'Add environment'`, primary `'Add'`, calling `widget.state.addEnvEntry(widget.item, AddEnvInput(name: _envName.text.trim(), apiBase: …, orgId: …, authKind: …, token/…: …))`; validate env name non-empty + org id numeric). Add `RemoveEnvDialog` (model on `RemoveDialog`; title `'Remove environment?'`, primary `'Remove'`, body: `'Removes the "<env>" environment and its local files from "<project>". If it is the last environment, the whole project is moved to the Trash.'`). Both take the needed `ProjectItem`/`EnvSummary`.

- [ ] **Step 5: Verify** — `cd desktop && flutter test test/app_state_env_mgmt_test.dart` → PASS. (`dart analyze lib/` will flag `home_page.dart` where `selectProject` was assumed to pin an env / where `_ProjectView` doesn't exist yet — that is Task 3. Do not run full analyze here; the env-mgmt test doesn't import `home_page.dart`.)

- [ ] **Step 6: Commit** — `git add desktop/lib/src/app_state.dart desktop/lib/src/dialogs.dart desktop/test/app_state_env_mgmt_test.dart && git commit -m "feat(desktop): AppState env management + project-node selects the Project view"`

---

### Task 3: `_ProjectView` UI + wire project-node selection

**Files:**
- Modify: `desktop/lib/src/home_page.dart`
- Test: `desktop/test/project_view_test.dart` (new)

**Interfaces:**
- Consumes: `AppState` (`selected`, `selectedEnv`, `selectProject`, `selectEnv`, `syncEnvItem`, `addEnvEntry`, `removeEnvEntry`, `envKey`), `AddEnvDialog`/`RemoveEnvDialog`/`EditConnectionDialog`.
- Produces: when a project node is selected (`selectedEnv == null`) the main pane shows `_ProjectView`; env child still shows `_ConnMain`.

- [ ] **Step 1: Failing widget test** — `desktop/test/project_view_test.dart`: seed a project with envs `['main','prod']`, `selectProject` it, pump `MdhScaffold(state:, view: NavView.connection, onSelectEnv: …, onSelectProject: …, onAddEnv: …, onRemoveEnv: …, …)`, expect the Project view renders: a row per env (find `'main'` and `'prod'` text in the table) and an "Add environment" affordance (`find.text('Add environment')`). Assert tapping the env row calls `selectEnv`.

- [ ] **Step 2: Verify failure** — `cd desktop && flutter test test/project_view_test.dart` → FAIL (no `_ProjectView`, `MdhScaffold` lacks the new callbacks).

- [ ] **Step 3: Implement** in `desktop/lib/src/home_page.dart`:
  - In `MdhScaffold`'s `NavView.connection` branch (currently `_ConnMain`), branch on selection: `state.selectedEnv == null ? _ProjectView(...) : _ConnMain(...)`. Add `MdhScaffold` callbacks `onAddEnv(ProjectItem)`, `onRemoveEnv(ProjectItem, EnvSummary)`, `onEditEnv(ProjectItem, EnvSummary)` (the existing edit path), and ensure a project-node tap routes to `selectProject` (it already does via `onSelectConn` — verify `_ProjectRow.onSelect` → `onSelectConn`).
  - Add `_ProjectView` (a StatelessWidget) modeled on the existing `_ConnMain`+`_OverviewPanel` styling and the `_FleetTable` table pattern:
    - Header bar (like `_ConnBar`): project name + folder + a "Sync all envs" button + "Add environment" button.
    - An **Environments table** (reuse the `_FleetTable`/`_FleetRow` cell helpers' visual style): one row per `env` in `state.selected!.summary.envs` — columns: env name, org id, host, files, last-sync/status badge (`_statusOf(state, item, env)` + `_StatusPill`/`_MiniBadge`), and a trailing action cluster (Sync / Edit / Remove). Row tap → `onSelectEnv(folder, env.name)`.
    - No promote section yet (Phase 3).
  - `_HomePageState`: wire `onAddEnv: (p) => showDialog(... AddEnvDialog(state: state, item: p))`, `onRemoveEnv: (p,e) async { if (await showDialog<bool>(...RemoveEnvDialog(env: e)) == true) _run(() => state.removeEnvEntry(p, e)); }`, `onEditEnv: (p,e) => showDialog(... EditConnectionDialog(state: state, item: p, env: e))`.

- [ ] **Step 4: Verify** — `cd desktop && dart analyze lib/` → clean; `flutter test test/project_view_test.dart test/app_state_env_mgmt_test.dart test/sidebar_multi_env_test.dart test/app_state_test.dart` → green. (Do NOT run full `flutter test` yet — the golden seeding needs the Task-4 update; the connection-view golden still pins an env so it renders `_ConnMain`, but a project-selected golden is added in Task 4.)

- [ ] **Step 5: Commit** — `git add desktop/lib/src/home_page.dart desktop/test/project_view_test.dart && git commit -m "feat(desktop): Project view (environments table + add/remove env)"`

---

### Task 4: Goldens + bridge integration test

**Files:**
- Modify: `desktop/test/golden_mdh_test.dart` (add a Project-view golden), regenerate `desktop/test/goldens/*.png`
- Modify: `desktop/integration_test/bridge_test.dart` (add `addEnv`/`removeEnv` cases)

- [ ] **Step 1** — In `golden_mdh_test.dart`, add a test `'project view — light'` that seeds a multi-env project, calls `selectProject(folder)` (env == null → Project view), and shoots `mdh_project_light.png`. Provide the new `MdhScaffold` callbacks as no-ops where a tapless render suffices.
- [ ] **Step 2** — Regenerate: `cd desktop && flutter test --update-goldens test/golden_mdh_test.dart`. Then run without `--update-goldens` to confirm green. Visually confirm `mdh_project_light.png` shows the environments table.
- [ ] **Step 3** — In `integration_test/bridge_test.dart`, add: create a project (`addProject`), `addEnv` a `prod` env (assert `listProjects` now shows `['main','prod']` and `secrets/prod.secrets.json` exists), `removeEnv('prod')` (assert back to `['main']`), then `removeEnv('main')` (assert returns null / project folder gone).
- [ ] **Step 4: Verify (whole suite)** — `cd desktop && flutter analyze` clean; `flutter test` all green; `cd desktop/rust && cargo test` green; attempt `flutter test integration_test -d macos` and report.
- [ ] **Step 5: Commit** — `git add desktop/test desktop/integration_test && git commit -m "test(desktop): Project-view golden + add/remove env bridge coverage"`

---

## Self-review notes
- Spec coverage: §6 add/edit/remove env → Tasks 1–3; §7.2 Project view (Environments table) → Task 3; selection model (project node = destination) → Task 2. Deferred to Phase 3 (as designed): the Promote panel inside the Project view. Deferred (documented): free-form first-env naming (`add_project` still `main`).
- Backward compat: single-env `main` projects render a one-row Project view; `remove_env` of the last env trashes the project (matches "removing the last env removes the project", spec §6).
- Type consistency: Rust `add_env(folder, AddEnvInput)` / `remove_env(folder, env) -> Option<ProjectSummary>` ↔ Dart `addEnv(folder:, input:)` / `removeEnv(folder:, env:)` (returns `ProjectSummary?`). `removeEnvEntry` handles the null (project-trashed) case.
