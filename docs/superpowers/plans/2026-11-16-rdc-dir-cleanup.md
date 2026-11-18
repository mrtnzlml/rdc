# Automatic `.rdc/` cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `rdc sync` and `rdc init` remove the `.rdc/` artifacts of envs that are no longer in `rdc.toml`, plus the obsolete `.rdc/cache`.

**Architecture:** One function, `state::orphans::prune_orphans`, lists `.rdc/state/` and `.rdc/conflicts/`, and removes entries whose env is not in the config. It goes through `crate::vfs`. The CLI entry points call it: `cli::sync::run`, `cli::sync::watch::run_watch` and `cli::init::run`. `run_cycle` does not call it, so the wasm/embed paths never reach it.

**Tech Stack:** Rust, `tempfile`, `assert_cmd`, `wiremock` (already dev-deps).

**Spec:** `docs/superpowers/specs/2026-11-16-rdc-dir-cleanup-design.md`

## Global Constraints

- All file access goes through `crate::vfs` (`tests/vfs_boundary.rs` enforces it).
- Never touch `.rdc/map/`, `.rdc/mapping.toml`, or any `.rdc/` entry whose shape is not listed below.
- An env name matches **case-insensitively** (`Dev.lock.json` belongs to env `dev`).
- If `.rdc/state/E.lock` exists and `EnvLock::try_acquire` returns `Ok(None)`, skip env `E` entirely.
- `dry_run` removes nothing.
- A removal failure is a warning on stderr. It never fails the command.
- No customer-specific names in code, tests or commits. Use `dev`/`test`/`prod`/`old`.

## Review Focus

1. **`rdc.toml` fails to parse:** the sync must not prune anything. It must not treat the config as "no envs" and wipe all state. The test goes in Task 2.
2. **Env name with a dot or suffix collision:** a file named `dev.lock.json.bak` or `dev.base.old` is not an rdc shape, so it is left alone. The test goes in Task 1.
3. **Orphan with only `E.lock.json` and no `E.lock`:** pruning must not create `E.lock` (from `try_acquire`) and then leave it behind. Call `try_acquire` only when `E.lock` already exists. The test goes in Task 1.
4. **The env being synced is never touched,** even when the env picker resolved it. It is in `rdc.toml` by construction, and the case-insensitive match covers its spelling. This is covered by Task 1's `keeps_known_envs`.
5. **Empty-dir removal:** `.rdc/state/` must not be removed while it still holds the current env's files. Use `vfs::remove_dir`, which fails on a non-empty dir, and ignore that error. The test goes in Task 1.

---

### Task 1: `prune_orphans`

**Files:**
- Create: `src/state/orphans.rs`
- Modify: `src/state/mod.rs` (add `pub mod orphans;`)
- Test: unit tests in `src/state/orphans.rs` (`#[cfg(test)] mod tests`, `tempfile::TempDir`)

**Interfaces:**
- Consumes: `crate::config::ProjectConfig` (`envs: BTreeMap<String, EnvConfig>`), `crate::cli::sync::lock::EnvLock::try_acquire(&Path) -> Result<Option<EnvLock>>`, `crate::vfs::{read_dir, remove_file, remove_dir, remove_dir_all, symlink_metadata, exists}`.
- Produces: `pub fn prune_orphans(root: &Path, cfg: &ProjectConfig, dry_run: bool) -> Vec<PathBuf>`. It returns the paths it removed, or would remove on a dry run, sorted. It prints one stderr line per path: `removed <rel>` or `would remove <rel>`, where `<rel>` is relative to `root`. On failure it prints a stderr warning, `could not remove <rel>: <err>`, and leaves that path out of the result.

Owned shapes, for an env name `E` (the part before the suffix, valid per `config::valid_env_name`):
- `.rdc/state/E.lock.json` (file), `.rdc/state/E.lock` (file), `.rdc/state/E.base` (dir)
- `.rdc/conflicts/E` (dir)
- `.rdc/cache` (file or dir), always removed.

`E` is orphaned when no key of `cfg.envs` equals it case-insensitively (`eq_ignore_ascii_case`).

- [ ] **Step 1: Write the failing tests**

Each test builds a temp project with a `ProjectConfig` whose `envs` holds `dev` only:
- `removes_all_shapes_of_an_unknown_env`: create `old.lock.json`, `old.lock`, `old.base/queues/x.json`, `conflicts/old/a.json` and `.rdc/cache/`. The result has exactly those 5 top-level paths, and none of them exists afterwards.
- `keeps_known_envs`: `dev.lock.json`, `dev.base/`, `conflicts/dev/` survive, and the result is empty.
- `matches_env_names_case_insensitively`: `Dev.lock.json` survives.
- `dry_run_removes_nothing`: same setup as the first test, `dry_run = true`. The result has the same 5 paths, and every one still exists.
- `ignores_unknown_shapes`: `dev.lock.json.bak`, `notes.txt`, `.rdc/mapping.toml`, `.rdc/map/old-to-dev.toml` and `state/old.base.old/` all survive.
- `does_not_create_a_lock_file`: only `old.lock.json` exists. After pruning, neither `old.lock.json` nor `old.lock` exists.
- `skips_an_env_whose_lock_is_held`: create `old.lock` and hold `EnvLock::try_acquire(old.lock)` in the test. `old.lock.json` and `old.lock` survive, and the result is empty.
- `removes_emptied_dirs_only`: with only `old.*` in `state/` and `conflicts/`, both dirs are gone afterwards. With `dev.lock.json` also present, `state/` survives.
- `no_rdc_dir_is_a_no_op`: an empty root returns an empty result and creates nothing.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib state::orphans`
Expected: FAIL to compile (`prune_orphans` not defined).

- [ ] **Step 3: Implement `prune_orphans` in `src/state/orphans.rs`**

Use `vfs::symlink_metadata` to tell files from dirs, and do not follow a symlink. Remove a dir with `remove_dir_all` and a file with `remove_file`. Remove the emptied `state/` and `conflicts/` with `remove_dir`, and ignore its error. On the `--dry-run` path, skip the lock check (dry run holds nothing and creates nothing). Add a short doc comment that names the spec.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib state::orphans`
Expected: PASS, 9 tests.

- [ ] **Step 5: Commit**

```bash
git add src/state/orphans.rs src/state/mod.rs
git commit -m "feat(state): prune .rdc/ artifacts of envs not in rdc.toml"
```

### Task 2: Run the prune from `sync` and `init`, and document it

**Files:**
- Modify: `src/cli/sync/mod.rs` (`run`, before `EnvLock::acquire`)
- Modify: `src/cli/sync/watch.rs` (`run_watch`, once, before the first cycle)
- Modify: `src/cli/init.rs` (`run`, right after the `scaffold` vector is built; also the "What NOT to edit" list in the scaffolded CLAUDE.md template, around line 1252)
- Modify: `src/cli/mod.rs` (`sync` long-about)
- Test: `tests/cli_sync.rs`, `tests/cli_init.rs`

**Interfaces:**
- Consumes: `state::orphans::prune_orphans(root, cfg, dry_run)` from Task 1, and `ProjectConfig::load(&Path) -> Result<ProjectConfig>`.
- Produces: nothing new.

Wiring rule: load `rdc.toml` with `ProjectConfig::load(cwd.join("rdc.toml"))`. **Only on `Ok`** call `prune_orphans(&cwd, &cfg, dry_run)`. On `Err`, skip the prune and let the existing code report the config error. `run_watch` passes `dry_run = false`, and `init` passes `false`. In `init`, use the `cfg` it just saved.

- [ ] **Step 1: Write the failing tests**

Follow the setup of `sync_restores_the_per_env_scaffolds_but_not_on_a_dry_run` in `tests/cli_sync.rs` (MockServer, `rdc init --env dev=…`, token file, `cwd_lock()`):
- `sync_prunes_state_of_an_env_removed_from_rdc_toml`: create `.rdc/state/old.lock.json`, `.rdc/state/old.base/x.json` and `.rdc/conflicts/old/a.json`. A `sync::run("dev", …, dry_run=true, …)` leaves all three. A real `sync::run("dev", …)` removes all three. `.rdc/state/dev.lock.json` exists afterwards.
- `sync_does_not_prune_when_rdc_toml_is_broken`: create `.rdc/state/old.lock.json`, then overwrite `rdc.toml` with `not toml [`. `sync::run` returns `Err`, and `old.lock.json` still exists.

In `tests/cli_init.rs`:
- `init_prunes_state_of_an_env_removed_from_rdc_toml`: run `rdc init --env dev=https://example.invalid/api/v1:1`, the way existing init tests do. Then create `.rdc/state/old.lock.json` and run `rdc init --force`. The file is gone, and stderr contains `removed .rdc/state/old.lock.json`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test cli_sync prune && cargo test --test cli_sync broken && cargo test --test cli_init prunes`
Expected: the two prune tests FAIL on `exists()` assertions. The broken-toml test may already pass, and it guards the wiring.

- [ ] **Step 3: Wire the calls as described above**

- [ ] **Step 4: Update the docs**

- `src/cli/mod.rs`, the `sync` long-about: add one sentence. `rdc sync` also removes `.rdc/state/<env>.*` and `.rdc/conflicts/<env>/` for any env no longer in `rdc.toml`, and `.rdc/cache`.
- The scaffolded CLAUDE.md, "What NOT to edit": after the `.rdc/state/<env>.lock.json` bullet, add one line: "Removing an env from `rdc.toml` makes the next `rdc sync` or `rdc init` delete that env's files under `.rdc/`."

- [ ] **Step 5: Run the release gate**

Run: `cargo test --locked && cargo clippy --all-targets --locked -- -D warnings && cargo doc --no-deps --document-private-items --locked && cargo clippy --lib --locked --target wasm32-unknown-unknown -- -D warnings`
Expected: all pass. The wasm clippy confirms that `state::orphans` still builds for the browser library. Fix any golden/template test that pins the CLAUDE.md template or the long-about text by updating its expected text.

- [ ] **Step 6: Commit**

```bash
git add src/cli tests/cli_sync.rs tests/cli_init.rs
git commit -m "feat(sync): clean up orphaned .rdc/ state on sync and init"
```
