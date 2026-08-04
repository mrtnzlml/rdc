# Desktop — environment naming & Files-tab UX

**Status:** design (brainstorming complete, awaiting review)
**Date:** 2026-08-04
**Builds on:** the completed desktop multi-env + promote feature (`2026-08-03-desktop-multi-env-and-promote-design.md`), now on local `main`.
**Scope:** `desktop/` (bridge + Flutter) + one small, non-behavioral addition to the core mapping module. No CLI behavior changes.

## 1. Problem
Three related gaps in the desktop app's environment handling:
- **A.** New projects force the first environment to be named `main` (Phase-1 deferral). Users can't name it `sand`/`dev`/`prod`.
- **B.** There is no way to rename an existing environment from the app.
- **C.** The env view's **Files** tab browses the *project root* (rdc.toml, secrets/, .rdc/, envs/…) instead of dropping the user straight into the selected environment's snapshot.

## 2. Decisions (fixed during brainstorming)
- **A — first-env name field, empty & required.** The New-project dialog gets an `ENVIRONMENT NAME` field with **no default** (the user must choose). `add_project` reuses the existing `AddEnvInput` type (its `name` is the env name) and the now-redundant `AddConnectionInput` is retired.
- **B — rename via a second field in the env Edit dialog.** The existing Edit dialog stays dual-purpose: it keeps its project-`NAME` field and gains an editable `ENVIRONMENT NAME` field. Changing it renames the env.
- **C — Files tab nests into `envs/<selectedEnv>/`.** Landing directly in the env's snapshot; groundwork for later hiding the directory structure entirely.
- **Delivery:** one combined effort, executed via subagent-driven-development with per-task review.

## 3. Goals / non-goals
**Goals:** name the first env freely; rename any env safely and entirely locally; Files tab scoped to the env.
**Non-goals (this iteration):** fully hiding the on-disk directory structure (C is only the first step); a `rdc rename-env` CLI command (rename stays a desktop/manual concern); renaming the *project* folder gains no new behavior.

## 4. Verified facts (grounding — checked against the code, not memory)
- **An env name is tied to these on-disk locations** (`src/paths.rs`), all of which a rename must move `old → new`:
  - `envs/<env>/` (`env_root`) — carries *everything* env-local: snapshot, `overlay/`, `overlay.toml`, `organization.json`, hooks/queues/schemas/…
  - `secrets/<env>.secrets.json`, `.rdc/state/<env>.lock.json`, `.rdc/state/<env>.lock`, `.rdc/state/<env>.base/`, `.rdc/conflicts/<env>/`
  - the `[envs.<env>]` section key in `rdc.toml`
- **`.rdc/mapping.toml` keys rows by env name.** `GenericMapping` stores per-kind rows as `Vec<BTreeMap<env_name, slug>>` (`src/mapping.rs`). A rename must rewrite every row's `old` key to `new`. The CLI already *anticipates* renames: `GenericMapping::validate` only **warns** (doesn't error) on a row referencing an env absent from `rdc.toml` (comment: "envs get renamed").
- **No `rename_env` exists** anywhere in the core (`grep` clean) — rename is a manual op today.
- **`valid_env_name`** (`desktop/rust/src/api/rdc.rs`) already enforces `[A-Za-z0-9_-]+`, non-empty — reused for A and B.
- **`AddEnvInput`** (`{name, api_base, org_id, auth_kind, token, username, password}`) is byte-identical in shape to `AddConnectionInput`; only the meaning of `name` differs (env vs project) — so A can retire the latter.
- **The Files tab** (`_FilesPanel` in `_ConnMain`, shown when an env is selected) takes `rootFolder: item.summary.folder`; it already degrades on a read error. The Files-tab goldens (`mdh_files_light`, `mdh_files_preview_light`, `mdh_files_preview_json_light`) are seeded by `_filesState` with a fixture that currently puts previewable files (`mapping.toml`, `sample.json`) at the **project root** — those move under `envs/<env>/` for C.

## 5. A — custom first-environment name
- **Bridge:** replace `add_project(parent, AddConnectionInput)` with `add_project(parent: String, project_name: String, first_env: AddEnvInput) -> Result<ProjectSummary>`. Slug the folder from `project_name`; validate `first_env.name` with `valid_env_name` (reject empty/invalid with a clear message); write `[envs.<first_env.name>]` + credentials under that name. **Retire `AddConnectionInput`** (only `add_project` used it); regenerate FRB bindings.
- **Dart:** the New-project dialog adds an `ENVIRONMENT NAME` field (autofocus after project name, empty, `hint: 'e.g. prod'`), validates non-empty client-side, and builds `AddEnvInput`. `AppState.addProjectEntry(String projectName, AddEnvInput firstEnv)`; after add it selects the new project (Project view), unchanged.
- **Tests:** bridge round-trip creating `[envs.prod]` + `secrets/prod.secrets.json`; rejection of empty/invalid env name; the integration `bridge_test.dart` `addProject` calls updated to the new signature.

## 6. B — rename an existing environment
- **Core (`src/mapping.rs`):** add `impl GenericMapping { pub fn rename_env(&mut self, old: &str, new: &str) }` that, for every kind's rows, moves each row's `old` key to `new` (no-op when a row lacks `old`). Implemented against GenericMapping's actual kind set (implementer reads the module). Unit-tested. This is the only core change and is non-behavioral (a new method).
- **Bridge:** `rename_env(folder: String, old: String, new: String) -> Result<ProjectSummary>`:
  1. Validate: `valid_env_name(&new)`; the project has `old`; `new` != `old`; `new` is not already an env. Clear errors for each.
  2. **Filesystem moves** via `rdc::paths::Paths::for_env(folder, old|new)` accessors — `std::fs::rename` each path that exists: `env_root`, `secrets_file`, `lockfile`, `env_lock`, `base_cache_root`, and the `.rdc/conflicts/<env>/` dir. (Missing optional paths are skipped.)
  3. **Rewrite** `.rdc/mapping.toml` (if present): `GenericMapping::load` → `rename_env(old,new)` → `save`.
  4. **Rename** the `rdc.toml` `[envs.<old>]` section → `[envs.<new>]` **last** (the authoritative record that the env now exists as `new`), preserving `api_base`/`org_id`.
  5. Return `ProjectSummary::from(discover::inspect(folder))`.
  - **Ordering rationale:** files → mapping → rdc.toml. A failure before step 4 leaves `rdc.toml` naming `old` (consistent with a ret/inspect); step 4 is a single small write. Filesystem env-rename can't be fully atomic — non-atomic partial failure is recoverable (re-run / inspect) and is called out as a risk (§9).
- **Dart:** `EditConnectionDialog` gains an editable `ENVIRONMENT NAME` field pre-filled with `widget.env.name`. `AppState.editEnvEntry(item, env, input)` becomes rename-aware: if the submitted env name differs and is valid, call `renameEnv(folder, env.name, newName)` **first**, then `editProject(folder, newName, input)` for the connection + project-folder edit; re-select the (possibly renamed) env/project. **Guard:** the dialog/AppState refuses the rename when that env is mid-sync (`syncState[envKey] == running`), with a clear message.
- **Tests:** core `rename_env` mapping-key rewrite; bridge `rename_env` (moves every per-env path, rewrites mapping, rejects collision/invalid/unknown, preserves api_base/org_id); a mid-sync guard test at the AppState level; integration `bridge_test.dart` rename round-trip (create → addEnv → rename → assert `listProjects` + moved secrets/state).

## 7. C — Files tab scoped to the environment
- **Dart:** in `_ConnMain`, the Files tab's `rootFolder` becomes `<folder>/envs/<selectedEnv>` (joined with `Platform.pathSeparator`) instead of `<folder>`. `_FilesPanel` gains a not-exists check: when `envs/<env>/` doesn't exist yet (env never synced), show a friendly "This environment hasn't been synced yet." empty state rather than a read error.
- **Fixture/goldens:** `_filesState`'s fixture moves the previewable files under `envs/<env>/` — the TOML-preview and JSON-preview goldens tap files that now live in the env tree (e.g. `envs/main/schemas/invoices.json` for JSON, an `envs/main/…/*.toml` for TOML). Regenerate the 3 Files-tab goldens; the list-view golden now shows the env's contents (hooks/, queues/, …) rather than the project root.
- **Backward compat:** Files is only in the env view (an env is always selected there), so there is always a concrete `<env>` to scope to.

## 8. Backward compatibility
- Existing projects are untouched by A/C. B is opt-in and local-only (no remote calls); after a rename the CLI keeps working on the same folder (mapping rewrite keeps `migrate` consistent; the CLI already tolerates a not-yet-renamed mapping row).
- On-disk format is unchanged. FRB bindings regenerate + commit (A and B change the Rust `api`).

## 9. Risks / deferred
- **Env-rename is not fully atomic.** A crash mid-move can leave the project split across `old`/`new`. Mitigations: validate-first, small final `rdc.toml` write, mid-sync guard. A crash is recoverable by re-running the rename or by hand; documented, accepted for v1.
- **Concurrent CLI use:** if the CLI is mid-`sync` on the same env while the app renames it, files move under the CLI. The app guards its own syncs; cross-process (CLI) concurrency is the user's responsibility, same as today.
- **Deferred:** fully hiding the directory structure in the Files UI (C is step one); a first-class `rdc rename-env` CLI command.

## 10. Testing summary
Bridge cargo tests (add_project custom name + validation; rename_env moves/mapping/collision) + core mapping unit test (`rename_env`) + Flutter widget/unit tests (new-project env field, edit-dialog rename field + mid-sync guard, Files-tab env scoping) + regenerated goldens + integration `bridge_test.dart` (add_project new signature, rename round-trip). Whole suite (`flutter analyze`/`flutter test`/`cargo test`/`cargo test -p rdc`/`integration_test -d macos`) green before done.
