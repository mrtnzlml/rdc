# Store-Extension Anomaly Fix — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- []`) syntax for tracking.

**Goal:** Make rdc detect and repair hooks where `extension_source: "rossum_store"` but `hook_template: null` — the "store extension anomaly" — instead of just refusing to push.

**Architecture:** Three layered changes. (1) Pull warns when it sees an anomalous hook so the user notices early. (2) The existing push/deploy error message gains env, hook id, both cure names, and the repair command. (3) New `rdc repair <env> --fix-store-anomaly` interactively offers **Cure B** (one PATCH flipping `extension_source` to `"custom"`) or **Cure A** (`POST /hooks/create` with the right template URL, PATCH-mirror the old settings, swap `run_after` URLs on remote dependents, DELETE the old hook).

**Tech Stack:** Rust, anyhow, serde_json, reqwest (via existing `RossumClient`), wiremock for tests, clap for CLI.

**API facts (verified live against `api.elis.rossum.ai` on 2026-05-22):**
- `PATCH /hooks/<id>` with `hook_template` set returns HTTP 200 but **silently drops** the field. Confirmed both alone and paired with `extension_source`. So Cure A's restore-the-link path is impossible via PATCH; it must go through `POST /hooks/create`.
- `PATCH /hooks/<id> {"extension_source": "custom"}` returns 200 and the change persists on GET. Cure B is one call.
- `PATCH /hooks/<id> {"extension_source": "rossum_store"}` on a previously-`custom` hook also returns 200 — the anomaly is trivially created by any client that sets the marker without going through the install endpoint.

---

## File Structure

**New files:**
- `src/cli/repair/mod.rs` — split from existing `src/cli/repair.rs`, hosts the `run` dispatcher
- `src/cli/repair/rebuild_lock.rs` — extracted `rebuild_lock_run`
- `src/cli/repair/rename_slugs.rs` — extracted `rename_slugs_run` (wrapper around existing realign module)
- `src/cli/repair/store_anomaly.rs` — the new fix command (Cure A + Cure B + prompt)

**Modified files:**
- `src/cli/mod.rs` — add `--fix-store-anomaly` flag to `Command::Repair`, wire dispatch
- `src/cli/pull/hooks.rs` — emit `Action::Warn` for each anomalous hook the pull writes
- `src/cli/deploy/store_extensions.rs` — `check_store_extension_anomaly` takes env + optional hook id and emits a richer message

**Deleted:** `src/cli/repair.rs` (replaced by the module).

**New tests:**
- `tests/cli_repair.rs` — extend with `--fix-store-anomaly` integration tests (Cure B happy path; Cure A happy path; non-TTY refuses; ambiguous template surfaces; orphan adoption)
- Unit tests inline in `src/cli/repair/store_anomaly.rs` for pure helpers (template matcher, dependent-rewire body builder)
- `src/cli/pull/hooks.rs` already has tests in `tests/cli_sync.rs`; add one for the new warning

---

## Task 1: Improve `check_store_extension_anomaly` error message

The current error is `hooks/<slug>.json: marked as store extension (extension_source = rossum_store) but missing hook_template URL; refusing to push`. It doesn't name the env, doesn't list cures, and gives the user no command to run.

**Files:**
- Modify: `src/cli/deploy/store_extensions.rs:197-204` (function signature + body)
- Modify: `src/cli/deploy/store_extensions.rs:71` (call site in `plan_store_extension_bootstrap` — pass env label)
- Modify: `src/cli/push/hooks.rs:123` (call site in push create branch — pass env)
- Modify: `src/cli/deploy/store_extensions.rs:349-378` (existing unit tests — update signatures)

- [ ] **Step 1: Write the failing test (extend existing test)**

Replace the body of `check_anomaly_rejects_store_extension_without_template` in `src/cli/deploy/store_extensions.rs`:

```rust
#[test]
fn check_anomaly_rejects_store_extension_without_template() {
    let payload = serde_json::json!({
        "id": 12345, "url": "u", "name": "x", "type": "webhook",
        "extension_source": "rossum_store"
    });
    let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
    let err = check_store_extension_anomaly(&hook, "broken-slug", "prod").unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("broken-slug"), "names the slug: {msg}");
    assert!(msg.contains("prod"), "names the env: {msg}");
    assert!(msg.contains("12345"), "names the hook id: {msg}");
    assert!(msg.contains("hook_template"), "explains the problem: {msg}");
    assert!(msg.contains("rdc repair prod --fix-store-anomaly"),
        "points at the repair command: {msg}");
    assert!(msg.contains("Convert to custom") || msg.contains("convert to custom"),
        "names Cure B: {msg}");
    assert!(msg.contains("Reinstall") || msg.contains("reinstall"),
        "names Cure A: {msg}");
}

#[test]
fn check_anomaly_passes_for_regular_hook() {
    let payload = serde_json::json!({"id": 1, "url": "u", "name": "x", "type": "function", "extension_source": "custom"});
    let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
    assert!(check_store_extension_anomaly(&hook, "x", "dev").is_ok());
}

#[test]
fn check_anomaly_passes_for_store_extension_with_template() {
    let payload = serde_json::json!({
        "id": 1, "url": "u", "name": "x", "type": "webhook",
        "extension_source": "rossum_store",
        "hook_template": "https://x/api/v1/hook_templates/1"
    });
    let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
    assert!(check_store_extension_anomaly(&hook, "x", "dev").is_ok());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rdc check_anomaly`
Expected: 3 failures, the relevant one citing missing argument (`expected 2 arguments, found 3` for the new env param, and missing strings for the message asserts).

- [ ] **Step 3: Update the function**

Replace `src/cli/deploy/store_extensions.rs:194-204`:

```rust
/// Defensive guard: a hook with `extension_source: "rossum_store"` must
/// always have `hook_template` set. Production data violates this when a
/// client PATCHes `extension_source` to `"rossum_store"` without going
/// through `POST /hooks/create` — the API silently drops `hook_template`
/// on direct write but accepts the marker, leaving the hook in this
/// broken state. The fix is `rdc repair <env> --fix-store-anomaly`.
pub fn check_store_extension_anomaly(hook: &Hook, slug: &str, env: &str) -> Result<()> {
    if hook.is_store_extension() && hook.hook_template().is_none() {
        return Err(anyhow!(
            "hooks/{slug}.json (id {id}) on env '{env}': marked as store extension \
             (extension_source = rossum_store) but missing hook_template URL.\n\
             \n\
             Two fixes:\n\
               - Convert to custom (one PATCH, hook id preserved): the rossum_store\n\
                 tag was added in error; the hook isn't really a Store template instance.\n\
               - Reinstall as store extension (new hook id, dependents rewired): the\n\
                 hook genuinely is a Store template instance and the hook_template link\n\
                 should be restored.\n\
             \n\
             Run `rdc repair {env} --fix-store-anomaly` to choose interactively.",
            id = hook.id
        ));
    }
    Ok(())
}
```

- [ ] **Step 4: Update the two call sites**

`src/cli/deploy/store_extensions.rs` line 71 (inside `plan_store_extension_bootstrap`) — replace:

```rust
check_store_extension_anomaly(&hook, &slug)?;
```

with:

```rust
check_store_extension_anomaly(&hook, &slug, tgt_env_label)?;
```

`src/cli/push/hooks.rs` line 123 — replace:

```rust
crate::cli::deploy::store_extensions::check_store_extension_anomaly(&typed, slug)?;
```

with:

```rust
crate::cli::deploy::store_extensions::check_store_extension_anomaly(&typed, slug, env)?;
```

(The `env: &str` parameter is already in scope — see `push` function signature at line 57.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rdc check_anomaly`
Expected: 3 passes.

Run: `cargo build -p rdc`
Expected: clean build.

- [ ] **Step 6: Commit**

```bash
git add src/cli/deploy/store_extensions.rs src/cli/push/hooks.rs
git commit -m "fix(store-anomaly): richer error message with cure names and repair command"
```

---

## Task 2: Pull-time warning for anomalous hooks

Pull currently writes the anomalous hook to disk silently (`src/cli/pull/hooks.rs:36`). The first time the user notices is when `rdc push` or `rdc deploy` fails. Add a `Warn` event so the issue surfaces at pull.

**Files:**
- Modify: `src/cli/pull/hooks.rs:36-46` (the loop over `&hooks`)
- Test: `tests/cli_sync.rs` (new test using wiremock)

- [ ] **Step 1: Write the failing test**

Append to `tests/cli_sync.rs`:

```rust
#[tokio::test]
async fn pull_warns_on_anomalous_store_extension() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    // Override /hooks with one anomalous result.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 42,
                "url": format!("{}/api/v1/hooks/42", server.uri()),
                "name": "Broken Store Hook",
                "type": "webhook",
                "queues": [],
                "events": [],
                "config": {},
                "extension_source": "rossum_store",
                "hook_template": null
            }]
        })))
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert().success()
        .stderr(predicate::str::contains("broken-store-hook"))
        .stderr(predicate::str::contains("hook_template"))
        .stderr(predicate::str::contains("--fix-store-anomaly"));
}
```

(`mount_minimal_pull` already exists at the top of `tests/cli_sync.rs`. If it isn't there but is in `tests/cli_repair.rs`, copy it over — keep it private to each test file rather than introduce a shared module.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rdc --test cli_sync pull_warns_on_anomalous_store_extension`
Expected: FAIL — stderr doesn't contain the expected text.

- [ ] **Step 3: Add the warning to pull**

In `src/cli/pull/hooks.rs`, modify the loop at line 36 to emit a warning when an anomalous hook is encountered. Insert after the `used_slugs.insert(slug.clone());` line (around line 41):

```rust
        // Surface the store-extension anomaly at pull time. The
        // anomaly is `extension_source: "rossum_store"` AND
        // `hook_template: null`. The API silently accepts the marker
        // when a client PATCHes it directly without going through
        // /hooks/create, so customer envs sometimes carry these.
        // Pull writes the hook as-is (round-trip fidelity); this
        // warning means the user finds out about it now instead of
        // when a future push or deploy refuses to proceed.
        if hook.is_store_extension() && hook.hook_template().is_none() {
            progress.event(crate::log::Action::Warn, &format!(
                "hook/{slug} (id {}): extension_source=rossum_store but hook_template is null — \
                 run `rdc repair {env} --fix-store-anomaly` to fix",
                hook.id,
                env = ctx.paths.env(),
            ));
        }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p rdc --test cli_sync pull_warns_on_anomalous_store_extension`
Expected: PASS.

Run: `cargo test -p rdc` (full suite)
Expected: all pass, no regressions.

- [ ] **Step 5: Commit**

```bash
git add src/cli/pull/hooks.rs tests/cli_sync.rs
git commit -m "feat(store-anomaly): warn during pull when an anomalous hook is detected"
```

---

## Task 3: Split `repair.rs` into a module

The existing `src/cli/repair.rs` is a single file with two modes. The new `--fix-store-anomaly` mode adds ~300 lines including Cure A's reinstall flow; the file would become unwieldy. Split into a module before adding the new mode so the dispatch surface stays clean.

**Files:**
- Create: `src/cli/repair/mod.rs` (dispatch — extracted from current `repair.rs`)
- Create: `src/cli/repair/rebuild_lock.rs` (extracted `rebuild_lock_run`)
- Create: `src/cli/repair/rename_slugs.rs` (extracted `rename_slugs_run`)
- Delete: `src/cli/repair.rs`

This task does NOT add new functionality — it's a pure move.

- [ ] **Step 1: Create the new module files**

Create `src/cli/repair/rebuild_lock.rs` with the body of the current `rebuild_lock_run` and the imports it needs:

```rust
use crate::config::ProjectConfig;
use crate::paths::Paths;
use anyhow::{anyhow, Context, Result};

/// Online repair: back up the existing lockfile and re-pull from
/// remote. Local snapshot files are overwritten with remote
/// contents; local edits not present on remote are LOST. The safety
/// net is whatever backup the user took before invoking repair
/// (e.g. via git).
pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;
    if !cfg.envs.contains_key(env) {
        return Err(anyhow!("env '{env}' is not defined in rdc.toml"));
    }

    let paths = Paths::for_env(&cwd, env);
    let lockfile_path = paths.lockfile();

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    if lockfile_path.exists() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut backup = lockfile_path.clone();
        let new_name = format!(
            "{}.bak.{now}",
            backup.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("lock.json"),
        );
        backup.set_file_name(new_name);
        std::fs::rename(&lockfile_path, &backup)
            .with_context(|| format!("backing up lockfile to {}", backup.display()))?;
        log.event(crate::log::Action::Repair, &format!("backed up lockfile to {}", backup.display()));
        log.event(crate::log::Action::Info, "rdc sync will now overwrite local snapshot files with remote contents");
    } else {
        log.event(crate::log::Action::Info, &format!("no existing lockfile at {}; proceeding with fresh sync", lockfile_path.display()));
    }

    crate::cli::sync::run(
        env, /* interactive */ false, /* dry_run */ false, /* diff */ false,
        /* allow_deletes */ false, /* no_push */ true, /* no_pull */ false,
    )
    .await?;
    log.event(crate::log::Action::Repair, &format!("done env '{env}' rebuilt"));
    Ok(())
}
```

Create `src/cli/repair/rename_slugs.rs`:

```rust
use anyhow::Result;

/// Offline repair: rename any local file whose slug no longer matches
/// its JSON `name` field. Delegates to the existing within-env realign
/// flow. No API calls.
pub async fn run(env: &str, check: bool, yes: bool) -> Result<()> {
    crate::cli::deploy::realign::run_within_env(env, check, yes).await
}
```

Create `src/cli/repair/mod.rs`:

```rust
//! `rdc repair <env>` — bring the local snapshot back into a clean state.
//!
//! Three modes, one mandatory:
//!
//! * `--rebuild-lock` (online): back up the existing lockfile and
//!   re-pull everything. Local edits LOST.
//! * `--rename-slugs` (offline): rename local files whose slug no
//!   longer matches their JSON `name`. Cascade-aware. No API calls.
//! * `--fix-store-anomaly` (online, interactive): repair hooks with
//!   `extension_source: "rossum_store"` and `hook_template: null`.

pub mod rebuild_lock;
pub mod rename_slugs;
// store_anomaly is added in Task 4.

use anyhow::{anyhow, Result};

pub async fn run(
    env: &str,
    rebuild_lock: bool,
    rename_slugs: bool,
    check: bool,
    yes: bool,
) -> Result<()> {
    // Pick exactly one mode. No implicit default because all modes
    // touch on-disk files (and `--fix-store-anomaly` also touches
    // the remote) in irreversible ways.
    match (rebuild_lock, rename_slugs) {
        (false, false) => Err(anyhow!(
            "rdc repair needs a mode flag: --rebuild-lock, --rename-slugs, or --fix-store-anomaly"
        )),
        (true, true) => Err(anyhow!(
            "rdc repair --rebuild-lock and --rename-slugs are mutually exclusive"
        )),
        (true, false) => {
            if check {
                return Err(anyhow!(
                    "rdc repair --rebuild-lock does not support --check (it always re-pulls). \
                     Use git to preview what a rebuild would overwrite."
                ));
            }
            rebuild_lock::run(env).await
        }
        (false, true) => rename_slugs::run(env, check, yes).await,
    }
}
```

- [ ] **Step 2: Delete the old single-file module**

Run: `git rm src/cli/repair.rs`

- [ ] **Step 3: Verify the build and existing tests still pass**

Run: `cargo build -p rdc`
Expected: clean build (no other module declares `repair`; the existing `pub mod repair;` in `src/cli/mod.rs:341` finds the directory module).

Run: `cargo test -p rdc --test cli_repair`
Expected: existing 4 tests pass unchanged.

- [ ] **Step 4: Commit**

```bash
git add src/cli/repair/ tests/
git commit -m "refactor(repair): split repair.rs into a module to make room for fix-store-anomaly"
```

---

## Task 4: Scaffold `--fix-store-anomaly` (detection + dry listing)

Add the CLI flag, dispatch, and a first stub that detects anomalous hooks in the local snapshot and prints them. No remote calls yet. This proves the wiring works end-to-end before the real cures land.

**Files:**
- Modify: `src/cli/mod.rs` (clap definition for `Repair`, dispatch)
- Create: `src/cli/repair/store_anomaly.rs`
- Modify: `src/cli/repair/mod.rs` (register module, route)
- Test: `tests/cli_repair.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/cli_repair.rs`:

```rust
#[tokio::test]
async fn fix_store_anomaly_lists_anomalous_hooks_then_exits_in_check_mode() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    // Override /hooks with two hooks: one anomalous, one clean.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [
                {
                    "id": 42, "url": format!("{}/api/v1/hooks/42", server.uri()),
                    "name": "Broken Store Hook", "type": "webhook",
                    "queues": [], "events": [], "config": {},
                    "extension_source": "rossum_store", "hook_template": null
                },
                {
                    "id": 43, "url": format!("{}/api/v1/hooks/43", server.uri()),
                    "name": "Healthy Hook", "type": "function",
                    "queues": [], "events": [], "config": {},
                    "extension_source": "custom", "hook_template": null
                }
            ]
        })))
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["repair", "dev", "--fix-store-anomaly", "--check"])
        .assert().success()
        .stderr(predicate::str::contains("broken-store-hook"))
        .stderr(predicate::str::contains("id 42"))
        .stderr(predicate::str::contains("hook_template").not().or(predicate::str::contains("1 anomalous hook")));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rdc --test cli_repair fix_store_anomaly_lists_anomalous_hooks_then_exits_in_check_mode`
Expected: FAIL — `--fix-store-anomaly` is not a recognized argument.

- [ ] **Step 3: Add the flag to clap**

In `src/cli/mod.rs`, find the `Repair` variant (around line 171) and add a new field. Replace the existing struct with:

```rust
    Repair {
        env: Option<String>,
        /// Re-pull from remote and reconstruct the lockfile. Backs up
        /// the existing one to `<name>.bak.<unix-ts>`. Destroys local
        /// edits not present on remote.
        #[arg(long = "rebuild-lock", conflicts_with_all = ["rename_slugs", "fix_store_anomaly"])]
        rebuild_lock: bool,
        /// Rename local files whose slug no longer matches their JSON
        /// `name` field. Offline (no API calls).
        #[arg(long = "rename-slugs", conflicts_with_all = ["fix_store_anomaly"])]
        rename_slugs: bool,
        /// Repair hooks with `extension_source: "rossum_store"` and
        /// `hook_template: null`. Interactive per hook: convert to
        /// custom (one PATCH) or reinstall as store extension (new
        /// hook id, dependents rewired).
        #[arg(long = "fix-store-anomaly")]
        fix_store_anomaly: bool,
        /// With `--rename-slugs` or `--fix-store-anomaly`: print the
        /// plan and exit without writing anything.
        #[arg(long)]
        check: bool,
    },
```

Update the destructuring in the dispatch at the bottom of `mod.rs` (around line 269):

```rust
        Some(Command::Repair { env, rebuild_lock, rename_slugs, fix_store_anomaly, check }) => {
            let env = crate::cli::env_picker::pick_env("Which env to repair?", env)?;
            with_401_retry(&env, || {
                crate::cli::repair::run(&env, rebuild_lock, rename_slugs, fix_store_anomaly, check, cli.yes)
            })
            .await
        }
```

- [ ] **Step 4: Wire the new flag through `repair::run` and add the stub module**

Update `src/cli/repair/mod.rs` to take `fix_store_anomaly: bool`:

```rust
pub mod rebuild_lock;
pub mod rename_slugs;
pub mod store_anomaly;

use anyhow::{anyhow, Result};

pub async fn run(
    env: &str,
    rebuild_lock: bool,
    rename_slugs: bool,
    fix_store_anomaly: bool,
    check: bool,
    yes: bool,
) -> Result<()> {
    match (rebuild_lock, rename_slugs, fix_store_anomaly) {
        (false, false, false) => Err(anyhow!(
            "rdc repair needs a mode flag: --rebuild-lock, --rename-slugs, or --fix-store-anomaly"
        )),
        (true, false, false) => {
            if check {
                return Err(anyhow!(
                    "rdc repair --rebuild-lock does not support --check (it always re-pulls). \
                     Use git to preview what a rebuild would overwrite."
                ));
            }
            rebuild_lock::run(env).await
        }
        (false, true, false) => rename_slugs::run(env, check, yes).await,
        (false, false, true) => store_anomaly::run(env, check, yes).await,
        _ => Err(anyhow!(
            "repair mode flags are mutually exclusive; pick one"
        )),
    }
}
```

Create `src/cli/repair/store_anomaly.rs`:

```rust
//! `rdc repair <env> --fix-store-anomaly` — repair hooks with
//! `extension_source: "rossum_store"` and `hook_template: null`.
//!
//! Two cures, picked interactively per hook:
//!
//! * **Convert to custom (Cure B)**: one `PATCH /hooks/<id>
//!   {"extension_source": "custom"}`. Hook id preserved; no
//!   rewiring; instant. Right answer when the rossum_store tag was
//!   added in error.
//!
//! * **Reinstall as store extension (Cure A)**: `POST /hooks/create`
//!   with the right template URL, `PATCH` the new hook to mirror
//!   the old settings, swap `run_after` URLs on every dependent,
//!   `DELETE` the old hook. New hook id. Right answer when the
//!   hook genuinely is a Store template instance.

use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::log::{Action, Log};
use crate::model::Hook;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;

/// Walk the local snapshot's `hooks/` directory and return every hook
/// with `extension_source: "rossum_store"` AND `hook_template: null`.
/// Returns `(slug, hook)` pairs sorted by slug.
pub fn find_anomalies(paths: &Paths) -> Result<Vec<(String, Hook)>> {
    let hooks_dir = paths.hooks_dir();
    let mut out = Vec::new();
    if !hooks_dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&hooks_dir)
        .with_context(|| format!("reading {}", hooks_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let slug = path.file_stem().and_then(|s| s.to_str())
            .unwrap_or("").to_string();
        let hook = crate::snapshot::hook::read_hook(&hooks_dir, &slug)?;
        if hook.is_store_extension() && hook.hook_template().is_none() {
            out.push((slug, hook));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

pub async fn run(env: &str, check: bool, yes: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg = ProjectConfig::load(&cwd.join("rdc.toml"))?;
    let env_cfg = cfg.envs.get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;
    let paths = Paths::for_env(&cwd, env);
    let log = Arc::new(Log::new(crate::cli::resolve::detect_color_mode(false)));

    let anomalies = find_anomalies(&paths)?;
    if anomalies.is_empty() {
        log.event(Action::Info, &format!("no anomalous store-extension hooks in env '{env}'"));
        return Ok(());
    }

    log.event(Action::Info, &format!(
        "{} anomalous hook(s) in env '{env}':",
        anomalies.len()
    ));
    for (slug, hook) in &anomalies {
        log.event(Action::Info, &format!("  hooks/{slug}  (id {}, name {:?}, type {})",
            hook.id, hook.name, hook.hook_type));
    }

    if check {
        return Ok(());
    }

    // Construct the client; subsequent tasks will use it for the cures.
    let token = resolve_token(&cwd, env)?;
    let _client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing API client")?;
    let _lockfile = Lockfile::load(&paths.lockfile())
        .with_context(|| format!("loading lockfile from {}", paths.lockfile().display()))?;
    let _ = yes; // wired in Task 5

    // Task 5 implements Cure B; Task 6 implements Cure A.
    Err(anyhow!(
        "the per-hook prompt is implemented in the next task; for now, use --check to list anomalies"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn find_anomalies_returns_only_store_extensions_missing_template() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_dir = tmp.path().join("envs/dev/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();

        let write = |slug: &str, payload: serde_json::Value| {
            std::fs::write(
                hooks_dir.join(format!("{slug}.json")),
                serde_json::to_string_pretty(&payload).unwrap(),
            ).unwrap();
        };

        write("anomalous", json!({
            "id": 42, "url": "u", "name": "Broken", "type": "webhook",
            "queues": [], "events": [], "config": {"private": true},
            "extension_source": "rossum_store"
        }));
        write("healthy-store", json!({
            "id": 43, "url": "u", "name": "OK Store", "type": "webhook",
            "queues": [], "events": [], "config": {"private": true},
            "extension_source": "rossum_store",
            "hook_template": "https://x/api/v1/hook_templates/1"
        }));
        write("custom", json!({
            "id": 44, "url": "u", "name": "Custom Hook", "type": "function",
            "queues": [], "events": [], "config": {},
            "extension_source": "custom"
        }));

        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let out = find_anomalies(&paths).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "anomalous");
        assert_eq!(out[0].1.id, 42);
    }

    #[test]
    fn find_anomalies_returns_empty_when_no_hooks_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        assert!(find_anomalies(&paths).unwrap().is_empty());
    }
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p rdc --test cli_repair fix_store_anomaly_lists_anomalous_hooks_then_exits_in_check_mode`
Expected: PASS.

Run: `cargo test -p rdc store_anomaly`
Expected: 2 unit tests pass.

Run: `cargo test -p rdc` (full suite)
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/cli/mod.rs src/cli/repair/ tests/cli_repair.rs
git commit -m "feat(repair): scaffold --fix-store-anomaly with detection and --check listing"
```

---

## Task 5: Implement Cure B (convert to custom)

Cure B is one PATCH per hook. Implement the interactive prompt with [c]/[r]/[s] choices, wire [c] end to end. [r] still returns "not yet implemented" — Task 6 lands it.

**Files:**
- Modify: `src/cli/repair/store_anomaly.rs` (replace the Task-4 stub `run` body)
- Modify: `src/cli/resolve.rs` (add a small `prompt_anomaly_cure` helper next to existing prompts)
- Test: `tests/cli_repair.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/cli_repair.rs`:

```rust
#[tokio::test]
async fn fix_store_anomaly_cure_b_patches_extension_source_to_custom() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "id": 42, "url": format!("{}/api/v1/hooks/42", server.uri()),
                "name": "Broken", "type": "webhook",
                "queues": [], "events": [], "config": {},
                "extension_source": "rossum_store", "hook_template": null
            }]
        })))
        .mount(&server).await;

    // The PATCH the cure will issue. Capture and verify the body.
    let patched = std::sync::Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
    let patched_clone = patched.clone();
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/42"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = req.body_json().unwrap();
            *patched_clone.lock().unwrap() = body.clone();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 42, "url": format!("{}/api/v1/hooks/42", req.url.origin().ascii_serialization()),
                "name": "Broken", "type": "webhook",
                "queues": [], "events": [], "config": {},
                "extension_source": "custom", "hook_template": null,
                "modified_at": "2026-05-22T12:00:00.000000Z"
            }))
        })
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["sync", "dev", "--no-push"]).assert().success();

    // Non-interactive (`--yes`): default cure is convert-to-custom.
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["--yes", "repair", "dev", "--fix-store-anomaly"])
        .assert().success()
        .stderr(predicate::str::contains("hooks/broken (id 42) → converted to custom"));

    let body = patched.lock().unwrap().clone();
    assert_eq!(body, serde_json::json!({"extension_source": "custom"}));

    // Local snapshot reflects the change.
    let local: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.path().join("envs/dev/hooks/broken.json")).unwrap()
    ).unwrap();
    assert_eq!(local["extension_source"], "custom");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rdc --test cli_repair fix_store_anomaly_cure_b_patches_extension_source_to_custom`
Expected: FAIL — the command currently exits with "the per-hook prompt is implemented in the next task".

- [ ] **Step 3: Add the `prompt_anomaly_cure` helper**

In `src/cli/resolve.rs`, near other prompt helpers (search for `prompt_token_owner` for placement), add:

```rust
/// Cure choice for an anomalous store-extension hook. `Convert` is
/// the safe default (one PATCH, hook id preserved); `Reinstall` is
/// the heavier option (new id, dependents rewired); `Skip` leaves
/// it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnomalyCure {
    Convert,
    Reinstall,
    Skip,
}

/// Per-hook interactive prompt. Non-TTY → `Convert` (safe default).
pub fn prompt_anomaly_cure(slug: &str, hook: &crate::model::Hook, interactive: bool) -> anyhow::Result<AnomalyCure> {
    if !interactive {
        return Ok(AnomalyCure::Convert);
    }
    use inquire::Select;
    let private = hook.config.get("private").and_then(|v| v.as_bool()).unwrap_or(false);
    let has_code = hook.config.get("code").and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty()).unwrap_or(false);
    let signals = format!(
        "  signals: type={}, config.private={}, has config.code={}",
        hook.hook_type, private, has_code
    );
    let prompt = format!(
        "Cure for hooks/{slug} (id {})?\n  {:?}\n{}",
        hook.id, hook.name, signals
    );
    let options = vec![
        "[c] Convert to custom (one PATCH, id preserved)",
        "[r] Reinstall as store extension (new id, rewires dependents)",
        "[s] Skip this hook",
    ];
    let choice = Select::new(&prompt, options).prompt()
        .map_err(|e| anyhow::anyhow!("anomaly cure prompt: {e}"))?;
    Ok(match choice.chars().nth(1) {
        Some('c') => AnomalyCure::Convert,
        Some('r') => AnomalyCure::Reinstall,
        _ => AnomalyCure::Skip,
    })
}
```

If `inquire` isn't already a dependency, check `Cargo.toml`; if missing, use the same prompt library the existing `prompt_token_owner` uses. (Run `grep -n "prompt_token_owner\|use inquire\|use dialoguer" src/cli/resolve.rs` to discover the convention.)

- [ ] **Step 4: Implement Cure B in `store_anomaly.rs::run`**

Replace the `run` function in `src/cli/repair/store_anomaly.rs` with:

```rust
pub async fn run(env: &str, check: bool, yes: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg = ProjectConfig::load(&cwd.join("rdc.toml"))?;
    let env_cfg = cfg.envs.get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;
    let paths = Paths::for_env(&cwd, env);
    let log = Arc::new(Log::new(crate::cli::resolve::detect_color_mode(false)));

    let anomalies = find_anomalies(&paths)?;
    if anomalies.is_empty() {
        log.event(Action::Info, &format!("no anomalous store-extension hooks in env '{env}'"));
        return Ok(());
    }

    log.event(Action::Info, &format!(
        "{} anomalous hook(s) in env '{env}':",
        anomalies.len()
    ));
    for (slug, hook) in &anomalies {
        log.event(Action::Info, &format!("  hooks/{slug}  (id {}, name {:?}, type {})",
            hook.id, hook.name, hook.hook_type));
    }

    if check {
        return Ok(());
    }

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing API client")?;
    let mut lockfile = Lockfile::load(&paths.lockfile())
        .with_context(|| format!("loading lockfile from {}", paths.lockfile().display()))?;

    let interactive = crate::cli::resolve::is_interactive(yes);

    let mut fixed = 0usize;
    let mut skipped = 0usize;
    for (slug, hook) in anomalies {
        let cure = crate::cli::resolve::prompt_anomaly_cure(&slug, &hook, interactive)?;
        match cure {
            crate::cli::resolve::AnomalyCure::Skip => {
                log.event(Action::Skip, &format!("hooks/{slug} (id {})", hook.id));
                skipped += 1;
            }
            crate::cli::resolve::AnomalyCure::Convert => {
                convert_to_custom(&client, &mut lockfile, &paths, &slug, &hook, &log).await?;
                fixed += 1;
            }
            crate::cli::resolve::AnomalyCure::Reinstall => {
                // Task 6 implements this.
                return Err(anyhow!(
                    "Reinstall (Cure A) is not yet implemented; pick [c] convert or [s] skip"
                ));
            }
        }
    }

    lockfile.save(&paths.lockfile())
        .with_context(|| format!("saving lockfile to {}", paths.lockfile().display()))?;

    log.event(Action::Repair, &format!(
        "done env '{env}': {fixed} fixed, {skipped} skipped"
    ));
    Ok(())
}

/// Cure B — `PATCH /hooks/<id> {"extension_source": "custom"}`,
/// rewrite the local snapshot to match, update the lockfile entry.
/// Hook id is preserved; no rewiring needed.
async fn convert_to_custom(
    client: &RossumClient,
    lockfile: &mut Lockfile,
    paths: &Paths,
    slug: &str,
    hook: &Hook,
    log: &Arc<Log>,
) -> Result<()> {
    let body = serde_json::json!({"extension_source": "custom"});
    let updated = client.update_hook_value(hook.id, &body, Some(log.clone())).await
        .with_context(|| format!("PATCH /hooks/{} (cure B for hooks/{slug})", hook.id))?;

    // Reuse the pull side's canonical serialization so disk + hash
    // stay aligned with what a future pull would write.
    let (json, code) = crate::snapshot::hook::serialize_hook(&updated)?;
    let overlay = crate::overlay::Overlay::load(&paths.overlay_file())?;
    let stripped = crate::cli::pull::common::maybe_strip_overlay(
        json,
        overlay.as_ref().and_then(|o| o.hook(slug)),
    )?;
    let hash = crate::state::hook_combined_hash(&stripped, &code);
    let local_path = paths.hooks_dir().join(format!("{slug}.json"));
    crate::snapshot::writer::write_atomic(&local_path, &stripped)
        .with_context(|| format!("writing post-cure snapshot to {}", local_path.display()))?;

    // The hook's `extension_source` lives in `extra` (flattened in the
    // model), so the typed update + serialize cycle above already
    // round-trips the new value. Lockfile entry id/url unchanged; only
    // content_hash + modified_at move.
    let prior = lockfile.objects.get("hooks").and_then(|m| m.get(slug)).cloned()
        .unwrap_or_else(|| crate::state::ObjectEntry {
            id: updated.id, url: Some(updated.url.clone()),
            modified_at: None, content_hash: None, secrets_hash: None,
        });
    lockfile.upsert("hooks", slug, crate::state::ObjectEntry {
        id: updated.id,
        url: Some(updated.url.clone()),
        modified_at: updated.modified_at().map(|s| s.to_string()),
        content_hash: Some(hash),
        secrets_hash: prior.secrets_hash,
    });
    log.event(Action::Repair, &format!("hooks/{slug} (id {}) → converted to custom", updated.id));
    Ok(())
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p rdc --test cli_repair fix_store_anomaly_cure_b_patches_extension_source_to_custom`
Expected: PASS.

Run: `cargo test -p rdc`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/cli/repair/store_anomaly.rs src/cli/resolve.rs tests/cli_repair.rs
git commit -m "feat(repair): implement Cure B (convert anomalous hook to custom)"
```

---

## Task 6: Implement Cure A (reinstall as store extension)

Cure A is the heavier path: `POST /hooks/create` to install fresh, PATCH the new hook with the old hook's settings, swap `run_after` URLs on every remote hook that referenced the old URL, DELETE the old hook, reconcile local snapshot.

The empirical answer from the live API test is that this is the **only** way to get `hook_template` set on a hook — PATCH silently drops it. The runbook §4 describes the exact same sequence.

**Files:**
- Modify: `src/cli/repair/store_anomaly.rs` (new helpers `match_template`, `reinstall_as_store_extension`, `rewire_dependents`)
- Test: `tests/cli_repair.rs`

- [ ] **Step 1: Write the failing test for the template matcher**

Append to the `tests` mod at the bottom of `src/cli/repair/store_anomaly.rs`:

```rust
    #[test]
    fn match_template_picks_unique_by_name_and_type() {
        use crate::model::HookTemplate;
        let templates: Vec<HookTemplate> = serde_json::from_value(json!([
            {"url": "https://x/api/v1/hook_templates/1", "name": "Master Data Hub",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"},
            {"url": "https://x/api/v1/hook_templates/2", "name": "Email Notifications",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
        ])).unwrap();
        let hook: Hook = serde_json::from_value(json!({
            "id": 1, "url": "u", "name": "Master Data Hub", "type": "webhook",
            "extension_source": "rossum_store"
        })).unwrap();
        let m = match_template(&hook, &templates).unwrap();
        assert_eq!(m.url, "https://x/api/v1/hook_templates/1");
    }

    #[test]
    fn match_template_errors_on_zero_matches() {
        use crate::model::HookTemplate;
        let templates: Vec<HookTemplate> = vec![];
        let hook: Hook = serde_json::from_value(json!({
            "id": 1, "url": "u", "name": "Mystery Hook", "type": "webhook",
            "extension_source": "rossum_store"
        })).unwrap();
        let err = match_template(&hook, &templates).unwrap_err();
        assert!(format!("{err:#}").contains("Mystery Hook"));
        assert!(format!("{err:#}").contains("not available"));
    }

    #[test]
    fn match_template_errors_on_ambiguous() {
        use crate::model::HookTemplate;
        let templates: Vec<HookTemplate> = serde_json::from_value(json!([
            {"url": "https://x/api/v1/hook_templates/1", "name": "MDH",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"},
            {"url": "https://x/api/v1/hook_templates/2", "name": "MDH",
             "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
        ])).unwrap();
        let hook: Hook = serde_json::from_value(json!({
            "id": 1, "url": "u", "name": "MDH", "type": "webhook",
            "extension_source": "rossum_store"
        })).unwrap();
        let err = match_template(&hook, &templates).unwrap_err();
        assert!(format!("{err:#}").contains("ambiguous"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rdc store_anomaly::tests::match_template`
Expected: FAIL — `match_template` not defined.

- [ ] **Step 3: Implement the template matcher**

Add to `src/cli/repair/store_anomaly.rs`:

```rust
/// Pick the single tgt template matching `(name, type, "rossum_store")`
/// for an anomalous hook. Errors describe what the user must do to
/// disambiguate. Mirrors `build_template_url_map` in
/// `cli::deploy::store_extensions` but for the single-env case where
/// the user is repairing an existing hook.
pub fn match_template<'a>(
    hook: &Hook,
    templates: &'a [crate::model::HookTemplate],
) -> Result<&'a crate::model::HookTemplate> {
    let key = (hook.name.as_str(), hook.hook_type.as_str(), "rossum_store");
    let matches: Vec<&crate::model::HookTemplate> = templates.iter()
        .filter(|t| (t.name.as_str(), t.template_type.as_str(), t.extension_source.as_str()) == key)
        .collect();
    match matches.len() {
        0 => Err(anyhow!(
            "template matching ({:?}, type={}, extension_source=rossum_store) is not available on this env. \
             Either install the template manually via the Rossum UI then re-run, or pick the Convert cure if the hook isn't really a Store template instance.",
            hook.name, hook.hook_type
        )),
        1 => Ok(matches[0]),
        n => Err(anyhow!(
            "ambiguous templates for ({:?}, type={}) on this env ({n} matches: {}). \
             Manual intervention required — DELETE the anomalous hook and re-install via the Rossum UI with the right template.",
            hook.name, hook.hook_type,
            matches.iter().map(|t| t.url.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}
```

- [ ] **Step 4: Run the matcher tests to verify they pass**

Run: `cargo test -p rdc store_anomaly::tests::match_template`
Expected: 3 PASS.

- [ ] **Step 5: Write the integration test for the full Cure A flow**

Append to `tests/cli_repair.rs`:

```rust
#[tokio::test]
async fn fix_store_anomaly_cure_a_reinstalls_and_rewires_dependents() {
    let server = MockServer::start().await;
    mount_minimal_pull(&server).await;

    let anomalous_url = format!("{}/api/v1/hooks/42", server.uri());
    let dependent_url = format!("{}/api/v1/hooks/100", server.uri());
    let new_hook_url = format!("{}/api/v1/hooks/999", server.uri());

    // Phase 1: pull sees the anomalous hook + a dependent that references it.
    Mock::given(method("GET")).and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [
                {
                    "id": 42, "url": anomalous_url, "name": "Master Data Hub",
                    "type": "webhook", "queues": [], "events": ["annotation_content.initialize"],
                    "config": {"private": true},
                    "extension_source": "rossum_store", "hook_template": null
                },
                {
                    "id": 100, "url": dependent_url, "name": "Downstream",
                    "type": "function", "queues": [],
                    "events": ["annotation_content.initialize"],
                    "config": {"runtime": "python3.12", "code": "def f(p): return {}"},
                    "extension_source": "custom", "hook_template": null,
                    "run_after": [anomalous_url.clone()]
                }
            ]
        }))).mount(&server).await;

    // Templates available on the env.
    Mock::given(method("GET")).and(path("/api/v1/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [{
                "url": format!("{}/api/v1/hook_templates/39", server.uri()),
                "name": "Master Data Hub", "type": "webhook",
                "extension_source": "rossum_store", "install_action": "copy"
            }]
        }))).mount(&server).await;

    // POST /hooks/create returns the freshly installed hook (id 999).
    let install_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let install_clone = install_calls.clone();
    Mock::given(method("POST")).and(path("/api/v1/hooks/create"))
        .respond_with(move |req: &wiremock::Request| {
            install_clone.lock().unwrap().push(req.body_json().unwrap());
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 999, "url": new_hook_url.clone(),
                "name": "Master Data Hub", "type": "webhook",
                "queues": [], "events": ["annotation_content.initialize"],
                "config": {"private": true},
                "extension_source": "rossum_store",
                "hook_template": format!("{}/api/v1/hook_templates/39", server.uri())
            }))
        }).mount(&server).await;

    // PATCH /hooks/999 — the mirror step (Cure A).
    Mock::given(method("PATCH")).and(path("/api/v1/hooks/999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 999, "url": new_hook_url.clone(),
            "name": "Master Data Hub", "type": "webhook",
            "queues": [], "events": ["annotation_content.initialize"],
            "config": {"private": true},
            "extension_source": "rossum_store",
            "hook_template": format!("{}/api/v1/hook_templates/39", server.uri())
        }))).mount(&server).await;

    // PATCH /hooks/100 — dependent rewire. Capture the body.
    let dep_patches = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let dep_clone = dep_patches.clone();
    Mock::given(method("PATCH")).and(path("/api/v1/hooks/100"))
        .respond_with(move |req: &wiremock::Request| {
            dep_clone.lock().unwrap().push(req.body_json().unwrap());
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 100, "url": dependent_url.clone(),
                "name": "Downstream", "type": "function", "queues": [],
                "events": ["annotation_content.initialize"],
                "config": {"runtime": "python3.12", "code": "def f(p): return {}"},
                "extension_source": "custom", "hook_template": null,
                "run_after": [new_hook_url.clone()]
            }))
        }).mount(&server).await;

    // DELETE /hooks/42 — old hook gone.
    Mock::given(method("DELETE")).and(path("/api/v1/hooks/42"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#).unwrap();
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .args(["sync", "dev", "--no-push"]).assert().success();

    // Drive Cure A via an env var that selects the cure in non-TTY mode.
    // Default for --yes is Convert; setting RDC_REPAIR_CURE=reinstall flips it.
    Command::cargo_bin("rdc").unwrap().current_dir(project.path())
        .env("RDC_REPAIR_CURE", "reinstall")
        .args(["--yes", "repair", "dev", "--fix-store-anomaly"])
        .assert().success()
        .stderr(predicate::str::contains("hooks/master-data-hub").and(
            predicate::str::contains("reinstalled (new id 999)")));

    // Install body had the right template URL and token_owner (null in this fixture).
    let installs = install_calls.lock().unwrap();
    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0]["hook_template"],
        serde_json::json!(format!("{}/api/v1/hook_templates/39", server.uri())));

    // Dependent was rewired: run_after now points at the new URL.
    let deps = dep_patches.lock().unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0], serde_json::json!({"run_after": [new_hook_url.clone()]}));

    // Local snapshot of the slug now reflects the new id+url.
    let local: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.path().join("envs/dev/hooks/master-data-hub.json")).unwrap()
    ).unwrap();
    assert_eq!(local["extension_source"], "rossum_store");
    assert!(local["hook_template"].as_str().unwrap().contains("/hook_templates/39"));
}
```

The `RDC_REPAIR_CURE` env var is a deliberate test seam: in non-TTY mode (`--yes`), the default cure is Convert; this env var lets the integration test exercise the Reinstall path without faking a TTY. Document it in the help text so users in CI can opt in too.

- [ ] **Step 6: Run the test to verify it fails**

Run: `cargo test -p rdc --test cli_repair fix_store_anomaly_cure_a_reinstalls_and_rewires_dependents`
Expected: FAIL — Cure A returns the "not yet implemented" error.

- [ ] **Step 7: Implement Cure A in `store_anomaly.rs`**

Add the following functions to `src/cli/repair/store_anomaly.rs`, and update `run()` to dispatch to `reinstall_as_store_extension` when the cure is `Reinstall`:

```rust
/// Cure A — reinstall the hook properly via `POST /hooks/create`,
/// PATCH it with the old settings, swap `run_after` URLs on every
/// remote dependent, DELETE the old hook, reconcile the local
/// snapshot. New hook id; brief overlap.
async fn reinstall_as_store_extension(
    client: &RossumClient,
    lockfile: &mut Lockfile,
    paths: &Paths,
    slug: &str,
    old_hook: &Hook,
    log: &Arc<Log>,
) -> Result<()> {
    // 1. List templates on this env, match (name, type, "rossum_store").
    let templates = client.list_hook_templates(Some(log.clone())).await
        .context("listing hook templates for Cure A")?;
    let template = match_template(old_hook, &templates)?;

    // 2. List existing hooks for the orphan check (resumes a previously
    //    interrupted install before re-POSTing).
    let remote_hooks = client.list_hooks(Some(log.clone())).await
        .context("listing hooks for orphan check")?;
    let installed_id = match crate::cli::deploy::store_extensions::find_orphan(
        &remote_hooks, &old_hook.name, &template.url,
    ) {
        Some(orphan) => {
            log.event(Action::Info, &format!(
                "hooks/{slug}: adopting orphan id {} (previous install was interrupted)",
                orphan.id
            ));
            orphan.id
        }
        None => {
            // 3. Build install body. token_owner falls back to the old
            //    hook's value; if null, propagate null — the API will
            //    accept null and the user can set it later via the
            //    Rossum UI or a subsequent rdc sync from a richer overlay.
            let token_owner = old_hook.extra.get("token_owner").cloned()
                .unwrap_or(serde_json::Value::Null);
            let install_body = serde_json::json!({
                "name": old_hook.name,
                "hook_template": template.url,
                "events": old_hook.events,
                "queues": old_hook.queues,
                "token_owner": token_owner,
            });
            let installed = client.create_hook_via_install(&install_body, Some(log.clone())).await
                .with_context(|| format!("POST /hooks/create (Cure A for hooks/{slug})"))?;
            log.event(Action::Info, &format!(
                "hooks/{slug}: installed new id {} from template {:?}",
                installed.id, template.name
            ));
            installed.id
        }
    };

    // 4. PATCH the new hook with the old hook's mutable settings. Skip
    //    read-only fields (hook_template, extension_source, id, url,
    //    created_*, modified_*) — the API drops them anyway, but
    //    sending the minimal mirror is clearer.
    let mut patch_body = serde_json::Map::new();
    for field in ["settings", "active", "run_after", "sideload", "description", "metadata"] {
        if let Some(v) = old_hook.extra.get(field).cloned() {
            patch_body.insert(field.to_string(), v);
        } else if field == "active" {
            // active defaults to true on the typed struct; copy explicitly so
            // the new hook respects whatever the old one had.
            patch_body.insert(field.to_string(),
                serde_json::to_value(old_hook.extra.get("active").cloned()).unwrap_or(serde_json::json!(true)));
        }
    }
    if !old_hook.config.is_null() && !old_hook.config.as_object().map(|m| m.is_empty()).unwrap_or(true) {
        patch_body.insert("config".into(), old_hook.config.clone());
    }
    let updated = client.update_hook_value(installed_id, &serde_json::Value::Object(patch_body), Some(log.clone())).await
        .with_context(|| format!("PATCH /hooks/{installed_id} (mirror old settings)"))?;

    // 5. Rewire every remote hook whose run_after references the old URL.
    let old_url = old_hook.url.clone();
    let new_url = updated.url.clone();
    let mut rewired = 0usize;
    for h in &remote_hooks {
        let run_after = h.extra.get("run_after")
            .and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if !run_after.iter().any(|v| v.as_str() == Some(&old_url)) {
            continue;
        }
        let new_run_after: Vec<serde_json::Value> = run_after.into_iter()
            .map(|v| if v.as_str() == Some(&old_url) {
                serde_json::Value::String(new_url.clone())
            } else { v })
            .collect();
        let body = serde_json::json!({ "run_after": new_run_after });
        client.update_hook_value(h.id, &body, Some(log.clone())).await
            .with_context(|| format!("PATCH /hooks/{} (rewiring run_after)", h.id))?;
        rewired += 1;
    }
    if rewired > 0 {
        log.event(Action::Info, &format!(
            "hooks/{slug}: rewired {rewired} dependent(s) to new URL"
        ));
    }

    // 6. DELETE the old hook (idempotent: 404 is fine — the install
    //    orphan path may have just adopted what looked like an orphan).
    let delete_url = format!("{}/hooks/{}", client.base_url(), old_hook.id);
    match client.delete(&delete_url, Some(log.clone())).await {
        Ok(_) => {}
        Err(e) if crate::api::anyhow_has_status(&e, 404) => {
            log.event(Action::Info, &format!(
                "hooks/{slug}: old id {} already gone (404 on DELETE — harmless)",
                old_hook.id
            ));
        }
        Err(e) => return Err(e).with_context(|| format!("DELETE /hooks/{}", old_hook.id)),
    }

    // 7. Reconcile local snapshot. The slug is preserved (name is
    //    unchanged); only id/url/content_hash move.
    let (json, code) = crate::snapshot::hook::serialize_hook(&updated)?;
    let overlay = crate::overlay::Overlay::load(&paths.overlay_file())?;
    let stripped = crate::cli::pull::common::maybe_strip_overlay(
        json, overlay.as_ref().and_then(|o| o.hook(slug))
    )?;
    let hash = crate::state::hook_combined_hash(&stripped, &code);
    let local_path = paths.hooks_dir().join(format!("{slug}.json"));
    crate::snapshot::writer::write_atomic(&local_path, &stripped)
        .with_context(|| format!("writing post-reinstall snapshot to {}", local_path.display()))?;
    if let Some(code_str) = &code {
        let ext = crate::snapshot::hook::hook_code_extension(&updated);
        crate::snapshot::hook::write_hook_code(&paths.hooks_dir(), slug, code_str, ext)
            .with_context(|| format!("writing hook code for {slug}"))?;
    }

    lockfile.upsert("hooks", slug, crate::state::ObjectEntry {
        id: updated.id,
        url: Some(updated.url.clone()),
        modified_at: updated.modified_at().map(|s| s.to_string()),
        content_hash: Some(hash),
        secrets_hash: None,
    });

    // For each rewired dependent, refresh its local snapshot + lockfile
    // entry by GETting it. This keeps the local snapshot byte-aligned
    // with what `rdc sync` would observe next.
    for h in &remote_hooks {
        let run_after_had_old = h.extra.get("run_after")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().any(|v| v.as_str() == Some(&old_url)))
            .unwrap_or(false);
        if !run_after_had_old { continue; }
        let Some(dep_slug) = lockfile.slug_for_id("hooks", h.id).map(|s| s.to_string()) else { continue; };
        let fresh = client.get_hook(h.id, Some(log.clone())).await
            .with_context(|| format!("GET /hooks/{} (post-rewire refresh)", h.id))?;
        let (j, c) = crate::snapshot::hook::serialize_hook(&fresh)?;
        let s = crate::cli::pull::common::maybe_strip_overlay(
            j, overlay.as_ref().and_then(|o| o.hook(&dep_slug))
        )?;
        let hh = crate::state::hook_combined_hash(&s, &c);
        let dep_path = paths.hooks_dir().join(format!("{dep_slug}.json"));
        crate::snapshot::writer::write_atomic(&dep_path, &s)?;
        if let Some(code_str) = &c {
            let ext = crate::snapshot::hook::hook_code_extension(&fresh);
            crate::snapshot::hook::write_hook_code(&paths.hooks_dir(), &dep_slug, code_str, ext)?;
        }
        let prior = lockfile.objects.get("hooks").and_then(|m| m.get(&dep_slug)).cloned();
        lockfile.upsert("hooks", &dep_slug, crate::state::ObjectEntry {
            id: fresh.id,
            url: Some(fresh.url.clone()),
            modified_at: fresh.modified_at().map(|s| s.to_string()),
            content_hash: Some(hh),
            secrets_hash: prior.and_then(|p| p.secrets_hash),
        });
    }

    log.event(Action::Repair, &format!(
        "hooks/{slug}: reinstalled (new id {}); old id {} removed; {} dependent(s) rewired",
        updated.id, old_hook.id, rewired,
    ));
    Ok(())
}
```

Also update the cure dispatch in `run()`:

```rust
            crate::cli::resolve::AnomalyCure::Reinstall => {
                reinstall_as_store_extension(&client, &mut lockfile, &paths, &slug, &hook, &log).await?;
                fixed += 1;
            }
```

And wire `RDC_REPAIR_CURE` in the non-TTY default of `prompt_anomaly_cure` (in `src/cli/resolve.rs`):

```rust
    if !interactive {
        let env_choice = std::env::var("RDC_REPAIR_CURE").unwrap_or_default();
        return Ok(match env_choice.as_str() {
            "reinstall" => AnomalyCure::Reinstall,
            "skip" => AnomalyCure::Skip,
            _ => AnomalyCure::Convert, // default
        });
    }
```

You'll need to add to `RossumClient` a `base_url()` accessor if one doesn't exist, and a `delete(url, progress)` method if not present. Inspect `src/api/mod.rs` first — `delete` likely already exists (look for `DELETE` usage in the existing `cli/deploy/apply.rs` or `cli/push/deletes.rs`); reuse the existing method rather than adding a parallel one. If the accessor is absent, add the simplest possible `pub fn base_url(&self) -> &str { &self.base_url }`.

- [ ] **Step 8: Run the integration test to verify it passes**

Run: `cargo test -p rdc --test cli_repair fix_store_anomaly_cure_a_reinstalls_and_rewires_dependents`
Expected: PASS.

Run: `cargo test -p rdc`
Expected: full suite passes.

- [ ] **Step 9: Commit**

```bash
git add src/cli/repair/store_anomaly.rs src/cli/resolve.rs src/api/mod.rs tests/cli_repair.rs
git commit -m "feat(repair): implement Cure A (reinstall as store extension + rewire dependents)"
```

---

## Task 7: Final integration check

Make sure all the pieces work together against the real fixture suite and that the new flag is documented in `--help`.

- [ ] **Step 1: Full test run**

Run: `cargo test -p rdc`
Expected: all pass, no warnings beyond pre-existing.

- [ ] **Step 2: Check the help text**

Run: `cargo run -p rdc -- repair --help`
Expected: lists `--rebuild-lock`, `--rename-slugs`, `--fix-store-anomaly`, `--check` with sensible descriptions; the three modes appear mutually exclusive.

- [ ] **Step 3: Manual smoke (optional, against a throwaway env)**

If you have a non-prod Rossum tenant handy, create a test hook the same way the verification did (`POST /hooks` + `PATCH {"extension_source": "rossum_store"}`), then run:

```bash
rdc sync <env> --no-push    # pulls; should print the new pull-time warning
rdc repair <env> --fix-store-anomaly --check    # lists the anomaly
rdc repair <env> --fix-store-anomaly             # interactive prompt → [c] → confirm fixed
```

- [ ] **Step 4: Commit the docs update (if any)**

If `docs/superpowers/specs/2026-05-13-store-extensions-design.md` should reference the new repair flag, add a one-paragraph "Recovery" section near the end. Otherwise skip this step.

```bash
git add docs/
git commit -m "docs(store-anomaly): note recovery path in store-extensions design"
```

---

## Self-review notes

- **Spec coverage:** The runbook's §1 (diagnosis), §2 (fork in road), §3 (pre-flight), §4 (Cure A), §5 (Cure B), §6 (multi-hook batches), §8 (rollback) all map to tasks above. Task 4 covers §3 (listing). Task 5 covers §5. Task 6 covers §4 + §6 + §8 (the orphan adoption is the rollback story). The runbook's §7 ("things to refuse to do") is encoded in the per-hook prompt + the non-TTY default + the explicit `RDC_REPAIR_CURE` opt-in for reinstall in CI.
- **Cure A coverage gap:** the runbook §4.3 mentions queue references in queue files might need rewiring. Confirmed by reading the code that this is NOT needed — `POST /hooks/create` with `queues: [...]` attaches the new hook to those queues, and the subsequent `DELETE` removes the old hook from them. No queue-file edits required.
- **Side-effect safety (runbook §4 ⚠️):** The plan does not auto-deactivate hooks before reinstall. The user's per-hook prompt surfaces `config.private` and `has config.code` so the operator can decide. A future enhancement could detect known side-effect templates (Export to SFTP, Send Email) and warn — left out of this plan as YAGNI until a customer hits it.
- **Type consistency:** `AnomalyCure` enum, `match_template`, `find_anomalies`, `convert_to_custom`, `reinstall_as_store_extension` all named consistently across tasks.
- **Placeholders:** None — every step has either complete code or an exact command. The `inquire` library check in Task 5 Step 3 has a fallback discovery command (`grep -n use inquire ...`); this is acceptable because the user's existing prompt choice is the right convention to follow.
