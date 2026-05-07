# rdc M10 — `rdc push` for Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the round-trip. `rdc push <env>` writes locally-edited hook JSON + `.py` code back to the Rossum API via PATCH. Conflict detection mirrors pull: if local has edits AND remote drifted since the last lockfile snapshot, the push is aborted for that hook (preserving remote, requiring user re-pull). Hooks are the most-edited kind in real implementations; M10 ships push for hooks only and documents the path to extending coverage.

**Architecture:** Push uses the same combined-hash approach as M9 schemas, applied to hooks: hash = SHA-256(json_without_code || 0x00 || "code" || 0x00 || code_bytes). Each pull updates the lockfile with this combined hash, so existing M9 snapshots will see a one-time false conflict on the first M10 pull as the hash format upgrades — re-running pull resolves it. The push driver iterates hooks in the local snapshot, computes the combined hash, compares to the lockfile (= "the remote state at last pull"), then fetches the actual remote to confirm no drift before sending. PATCH is single-phase (hooks reference queues by URL, but queue URLs don't change between pulls).

**Tech Stack:** Same as M9.

**Scope:**
- ✅ `rdc push <env>` command
- ✅ Hook JSON + `.py` push via PATCH
- ✅ Conflict detection (local edits AND remote drift = abort that hook with warning)
- ✅ Lockfile update from server response
- ✅ Combined hash for hooks (replaces M7's JSON-only hash)
- ❌ NOT push for other kinds (queues, schemas, rules, etc.) — future milestones
- ❌ NOT creates (POST) — only updates of existing objects
- ❌ NOT deletes — local removal is not detected as a server delete
- ❌ NOT two-phase send — hooks don't need it; M11+ kinds will
- ❌ NOT explicit verification step (server PATCH response is the verification)
- ❌ NOT overlays — M11

**End state of M10:**

```
$ # Edit a hook's code locally
$ vim envs/dev/hooks/validator-invoices.py

$ rdc push dev
Pushed 1 hook to env 'dev'

$ # Subsequent pull is a no-op (server now matches local)
$ rdc pull dev
Pulled ... 0 conflicts from env 'dev'
```

On conflict:

```
$ rdc push dev
warning: hooks/validator-invoices.json — remote has changed since last pull, skipping push
        run `rdc pull dev` to fetch latest remote, resolve, then push again
Pushed 0 hooks to env 'dev', 1 skipped (conflict)
```

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `src/state/lockfile.rs` | Modify | Add `hook_combined_hash(json_bytes, code: &Option<String>) -> String` |
| `src/snapshot/hook.rs` | Modify | Add `serialize_hook(&Hook) -> (json_bytes, code: Option<String>)` (no I/O) |
| `src/cli/pull/hooks.rs` | Modify | Use `hook_combined_hash` for content_hash; reads .py if present to include in hash |
| `src/api/mod.rs` | Modify | Add `RossumClient::update_hook(id, &Hook) -> Hook` (PATCH) |
| `src/cli/push/mod.rs` | Create | `rdc push <env>` orchestrator |
| `src/cli/push/hooks.rs` | Create | Push driver for hooks |
| `src/cli/mod.rs` | Modify | Add `Push` subcommand; declare push module |
| `tests/cli_push.rs` | Create | Integration tests for push (success, conflict) |
| `README.md` | Modify | Document push |

---

## Task 1: `hook_combined_hash` helper

**Files:**
- Modify: `src/state/lockfile.rs`

- [ ] **Step 1: Add the function + tests**

Append to `src/state/lockfile.rs` after `schema_combined_hash`:

```rust
/// Compute the combined hash for a hook: the post-extraction `<slug>.json`
/// bytes plus the extracted code (when present). Mirrors `schema_combined_hash`
/// in style but covers a single optional code blob, not a list of formulas.
///
/// ```text
/// SHA-256(
///     json_bytes
///     [|| 0x00 || "code" || 0x00 || code_bytes]   // present only when hook has code
/// )
/// ```
pub fn hook_combined_hash(json_bytes: &[u8], code: &Option<String>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(json_bytes);
    if let Some(code) = code {
        hasher.update([0u8]);
        hasher.update(b"code");
        hasher.update([0u8]);
        hasher.update(code.as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        write!(&mut hex, "{:02x}", b).expect("writing to String cannot fail");
    }
    hex
}
```

Inside `mod tests`:

```rust
    #[test]
    fn hook_combined_hash_no_code() {
        let h1 = hook_combined_hash(b"{}", &None);
        let h2 = hook_combined_hash(b"{}", &None);
        assert_eq!(h1, h2);
        // No code → equivalent to plain content_hash of the JSON.
        assert_eq!(h1, content_hash(b"{}"));
    }

    #[test]
    fn hook_combined_hash_with_code_differs_from_no_code() {
        let h_no = hook_combined_hash(b"{}", &None);
        let h_with = hook_combined_hash(b"{}", &Some("def x(): pass".to_string()));
        assert_ne!(h_no, h_with);
    }

    #[test]
    fn hook_combined_hash_changes_when_code_changes() {
        let json = b"{}";
        let h1 = hook_combined_hash(json, &Some("v1".to_string()));
        let h2 = hook_combined_hash(json, &Some("v2".to_string()));
        assert_ne!(h1, h2);
    }
```

- [ ] **Step 2: Re-export from `state/mod.rs`**

```rust
pub use lockfile::{content_hash, hook_combined_hash, schema_combined_hash, Lockfile, ObjectEntry, LOCKFILE_VERSION};
```

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib state::lockfile`
Expected: 14 tests pass (11 from M9 + 3 new).

- [ ] **Step 4: Commit**

```bash
git add src/state/
git commit -m "feat(state): hook_combined_hash function (json + code)"
```

---

## Task 2: `serialize_hook` helper

**Files:**
- Modify: `src/snapshot/hook.rs`

- [ ] **Step 1: Add `serialize_hook`**

Append to `src/snapshot/hook.rs` (after `write_hook` and before `write_hook_code`):

```rust
/// Serialize a hook to its on-disk byte form WITHOUT writing. Returns the JSON
/// bytes (post-extraction) and the optional extracted code string. Used by
/// pull/push drivers to compute `hook_combined_hash` before deciding whether
/// to write or send.
pub fn serialize_hook(hook: &Hook) -> Result<(Vec<u8>, Option<String>)> {
    let mut json_value = serde_json::to_value(hook)
        .context("serializing hook to value")?;

    let code = json_value
        .get_mut("config")
        .and_then(|c| c.as_object_mut())
        .and_then(|m| m.remove("code"))
        .and_then(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        });

    let bytes = serde_json::to_vec_pretty(&json_value)
        .context("serializing hook json")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    Ok((bytes, code))
}
```

- [ ] **Step 2: Add a test**

Inside `mod tests`:

```rust
    #[test]
    fn serialize_hook_returns_json_and_code() {
        let h = sample_hook();
        let (bytes, code) = serialize_hook(&h).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(!s.contains("def x"), "code should be extracted from json");
        assert_eq!(code.as_deref(), Some("def x():\n    return 1\n"));
    }
```

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib snapshot::hook`
Expected: all hook codec tests pass + 1 new.

- [ ] **Step 4: Commit**

```bash
git add src/snapshot/hook.rs
git commit -m "feat(snapshot): serialize_hook helper for combined-hash use"
```

---

## Task 3: Switch hooks pull driver to combined hash

**Files:**
- Modify: `src/cli/pull/hooks.rs`

- [ ] **Step 1: Update `hooks.rs` to use combined hash**

In `src/cli/pull/hooks.rs`, replace the body of the `for hook in &hooks` loop. The existing version computes hash over JSON only; the new version computes combined hash including any code.

Replace the entire `hooks.rs` file with:

```rust
use super::common::{apply_pull_action, decide_pull_action, record_object, PullAction, PullCtx};
use crate::slug::slugify_unique;
use crate::snapshot::hook::{serialize_hook, write_hook_code};
use crate::state::hook_combined_hash;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all hooks. Returns `(count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>) -> Result<(usize, usize)> {
    let hooks = ctx.client.list_hooks().await.context("listing hooks")?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    for hook in &hooks {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.hooks_dir())
                .with_context(|| format!("creating {}", ctx.paths.hooks_dir().display()))?;
            dir_created = true;
        }

        let slug = match ctx.lockfile.slug_for_id("hooks", hook.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&hook.name, &used_slugs),
        };
        used_slugs.insert(slug.clone());

        let (proposed_json, proposed_code) = serialize_hook(hook)?;

        // Compute LOCAL combined hash (read disk if present).
        let local_path = ctx.paths.hooks_dir().join(format!("{slug}.json"));
        let py_path = ctx.paths.hooks_dir().join(format!("{slug}.py"));
        let pre_local_json = if local_path.exists() {
            Some(std::fs::read(&local_path)
                .with_context(|| format!("reading {}", local_path.display()))?)
        } else {
            None
        };
        let pre_local_code = if py_path.exists() {
            Some(std::fs::read_to_string(&py_path)
                .with_context(|| format!("reading {}", py_path.display()))?)
        } else {
            None
        };

        let remote_combined_hash = hook_combined_hash(&proposed_json, &proposed_code);

        let base_hash = ctx
            .lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.clone());
        let action = match (base_hash.as_deref(), &pre_local_json) {
            (None, _) => PullAction::Write,
            (_, None) => PullAction::Write,
            (Some(base), Some(local_json)) => {
                let local_combined = hook_combined_hash(local_json, &pre_local_code);
                let local_matches = local_combined == base;
                let remote_matches = remote_combined_hash == base;
                match (local_matches, remote_matches) {
                    (true, _) => PullAction::Write,
                    (false, true) => PullAction::KeepLocal,
                    (false, false) => PullAction::Conflict,
                }
            }
        };

        if action == PullAction::Conflict {
            conflicts += 1;
        }

        let recorded_hash = match action {
            PullAction::Write => {
                let (_, _) = apply_pull_action(action, &local_path, &proposed_json, remote_combined_hash.clone())
                    .map(|h| (h, ()))?;
                if let Some(code) = &proposed_code {
                    write_hook_code(&ctx.paths.hooks_dir(), &slug, code)
                        .with_context(|| format!("writing hook code for '{}'", hook.name))?;
                } else {
                    // If hook has no code, remove any stale .py file.
                    if py_path.exists() {
                        std::fs::remove_file(&py_path)
                            .with_context(|| format!("removing stale {}", py_path.display()))?;
                    }
                }
                remote_combined_hash
            }
            PullAction::KeepLocal => {
                // Don't touch JSON or .py.
                let local_json = pre_local_json.as_ref().unwrap();
                hook_combined_hash(local_json, &pre_local_code)
            }
            PullAction::Conflict => {
                // Apply emits .json.remote; we additionally write .py.remote when remote has code.
                apply_pull_action(action, &local_path, &proposed_json, remote_combined_hash.clone())?;
                if let Some(code) = &proposed_code {
                    let py_remote_path = ctx.paths.hooks_dir().join(format!("{slug}.py.remote"));
                    crate::snapshot::writer::write_atomic(&py_remote_path, code.as_bytes())?;
                }
                let local_json = pre_local_json.as_ref().unwrap();
                hook_combined_hash(local_json, &pre_local_code)
            }
        };

        record_object(
            ctx.lockfile,
            "hooks",
            &slug,
            hook.id,
            Some(hook.url.clone()),
            hook.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
    }

    Ok((hooks.len(), conflicts))
}
```

The fiddly bit: the `apply_pull_action` helper returns the hash but for Write/Conflict cases we want the explicit `remote_combined_hash` so the lockfile stays in sync. The above code handles that by computing `recorded_hash` directly per action.

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: existing tests must pass. NOTE: the M7 hooks conflict test expects a `validator-invoices.json.remote` file on conflict and asserts no `.py.remote` was written — our new code WRITES a `.py.remote` for hooks with code. The existing test `re_pull_emits_remote_file_on_real_conflict` should still pass because it only checks the JSON .remote file exists; it doesn't assert .py.remote is absent. Verify.

If the build fails due to `(_, _)` destructure pattern, simplify to:

```rust
            PullAction::Write => {
                apply_pull_action(action, &local_path, &proposed_json, remote_combined_hash.clone())?;
                if let Some(code) = &proposed_code {
                    write_hook_code(&ctx.paths.hooks_dir(), &slug, code)
                        .with_context(|| format!("writing hook code for '{}'", hook.name))?;
                } else if py_path.exists() {
                    std::fs::remove_file(&py_path)
                        .with_context(|| format!("removing stale {}", py_path.display()))?;
                }
                remote_combined_hash
            }
```

- [ ] **Step 3: Commit**

```bash
git add src/cli/pull/hooks.rs
git commit -m "feat(cli): hook pull uses combined hash (json + code)"
```

---

## Task 4: `RossumClient::update_hook`

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `tests/api.rs`

- [ ] **Step 1: Add the PATCH method**

In `src/api/mod.rs`, inside `impl RossumClient { ... }`, after the existing `list_hooks` method, add:

```rust
    /// PATCH /hooks/{id} with the given hook body. Returns the server's
    /// authoritative response (which may include server-set fields like
    /// modified_at). The full Hook is sent including config.code.
    pub async fn update_hook(&self, id: u64, hook: &crate::model::Hook) -> Result<crate::model::Hook> {
        let url = format!("{}/hooks/{id}", self.base_url);
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", format!("token {}", self.token))
            .json(hook)
            .send()
            .await
            .with_context(|| format!("PATCH {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status: status.as_u16(), body }.into());
        }
        let value = resp
            .json::<crate::model::Hook>()
            .await
            .with_context(|| format!("decoding PATCH response from {url}"))?;
        Ok(value)
    }
```

- [ ] **Step 2: Add an integration test**

Append to `tests/api.rs`:

```rust
#[tokio::test]
async fn update_hook_patches_and_returns_response() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/1"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hook_1.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into()).unwrap();
    let hook: rdc::model::Hook = serde_json::from_value(fixture("hook_1.json")).unwrap();
    let updated = client.update_hook(1, &hook).await.unwrap();
    assert_eq!(updated.id, 1);
    assert_eq!(updated.name, "Validator: invoices");
}
```

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --test api`
Expected: 18 tests pass (17 + 1 new).

- [ ] **Step 4: Commit**

```bash
git add src/api/ tests/api.rs
git commit -m "feat(api): RossumClient::update_hook (PATCH)"
```

---

## Task 5: `cli::push` orchestrator + hooks driver

**Files:**
- Create: `src/cli/push/mod.rs`
- Create: `src/cli/push/hooks.rs`
- Modify: `src/cli/mod.rs`

- [ ] **Step 1: Create `cli/push/mod.rs`**

```rust
use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

mod hooks;

pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);

    let cfg = ProjectConfig::load(&paths.project_config())
        .with_context(|| format!("loading project config from {}", paths.project_config().display()))?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token)
        .context("constructing Rossum API client")?;

    let mut lockfile = Lockfile::load(&paths.lockfile())?;

    let (n_pushed, n_skipped) = hooks::push(&paths, &client, &mut lockfile).await
        .with_context(|| format!("pushing hooks for env '{env}'"))?;

    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)
        .with_context(|| format!("regenerating _index.md for env '{env}'"))?;

    let mut summary = format!("Pushed {} to env '{env}'",
        crate::cli::pull::common_pluralize(n_pushed, "hook", "hooks"));
    if n_skipped > 0 {
        summary.push_str(&format!(", {} skipped (conflict)", n_skipped));
    }
    println!("{summary}");
    Ok(())
}
```

Note: `crate::cli::pull::common_pluralize` doesn't exist — `pluralize` is in `cli::pull::common` but it's `pub(crate)` reachable via `crate::cli::pull::common::pluralize`. Use that name. Update the line to:

```rust
    let mut summary = format!("Pushed {} to env '{env}'",
        crate::cli::pull::common::pluralize(n_pushed, "hook", "hooks"));
```

But `cli::pull::common` is currently `mod common;` (private). Make it `pub(crate) mod common;` in `cli/pull/mod.rs` if not already public. Or expose `pluralize` via a re-export. Simplest: change the `mod common;` line in `cli/pull/mod.rs` to `pub mod common;`.

- [ ] **Step 2: Create `cli/push/hooks.rs`**

```rust
use crate::api::RossumClient;
use crate::paths::Paths;
use crate::snapshot::hook::{read_hook, serialize_hook};
use crate::state::{hook_combined_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

/// Push locally-edited hooks to the remote. For each hook in the local snapshot:
/// - Compute its combined hash and compare to the lockfile's content_hash.
/// - If equal: no local edits, skip silently.
/// - If different: local has edits.
///   - Fetch the remote to confirm it matches the lockfile (no drift since last pull).
///   - If remote drifted: emit a warning, skip this hook (count as conflict).
///   - If remote matches lockfile: PATCH the hook with the local content.
///   - On success, update the lockfile entry from the server's PATCH response.
///
/// Returns `(pushed, skipped_due_to_conflict)`.
pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let hooks_dir = paths.hooks_dir();
    if !hooks_dir.exists() {
        return Ok((0, 0));
    }

    // Iterate the local snapshot. For each <slug>.json file, check if it has
    // local edits relative to the lockfile.
    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let entries: Vec<_> = std::fs::read_dir(&hooks_dir)
        .with_context(|| format!("reading {}", hooks_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing {}", hooks_dir.display()))?;

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue; // .remote files from prior conflicts
        }

        // Read local hook from disk (this re-merges code from .py).
        let local_hook = read_hook(&hooks_dir, slug)
            .with_context(|| format!("reading local hook '{slug}'"))?;

        // Compute local combined hash.
        let (local_json, local_code) = serialize_hook(&local_hook)?;
        let local_combined = hook_combined_hash(&local_json, &local_code);

        // Look up base hash from lockfile.
        let base_hash = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(slug))
            .and_then(|e| e.content_hash.clone());

        let Some(base) = base_hash else {
            // No lockfile entry — would need create (POST). Out of M10 scope.
            eprintln!("warning: hooks/{slug}.json — no lockfile entry, skipping (creates not supported in M10)");
            skipped += 1;
            continue;
        };

        if local_combined == base {
            // No local edits.
            continue;
        }

        // Local has edits. Fetch remote to check for drift.
        let id = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(slug))
            .map(|e| e.id)
            .expect("base_hash existed so the entry exists");

        let remote_hooks = client.list_hooks().await
            .context("listing hooks to verify no drift before push")?;
        let remote_hook = remote_hooks.iter().find(|h| h.id == id);
        let Some(remote_hook) = remote_hook else {
            eprintln!("warning: hooks/{slug}.json — id {id} not found on remote, skipping");
            skipped += 1;
            continue;
        };

        let (remote_json, remote_code) = serialize_hook(remote_hook)?;
        let remote_combined = hook_combined_hash(&remote_json, &remote_code);

        if remote_combined != base {
            eprintln!(
                "warning: hooks/{slug}.json — remote has changed since last pull, skipping push (run `rdc pull` first)"
            );
            skipped += 1;
            continue;
        }

        // Safe to PATCH.
        let updated = client.update_hook(id, &local_hook).await
            .with_context(|| format!("PATCH /hooks/{id}"))?;

        // Update lockfile from server response.
        let (updated_json, updated_code) = serialize_hook(&updated)?;
        let updated_hash = hook_combined_hash(&updated_json, &updated_code);
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: updated.id,
                url: Some(updated.url.clone()),
                modified_at: updated.modified_at().map(|s| s.to_string()),
                content_hash: Some(updated_hash),
            },
        );
        pushed += 1;
    }

    Ok((pushed, skipped))
}
```

- [ ] **Step 3: Wire push into `cli/mod.rs`**

Replace `src/cli/mod.rs` with:

```rust
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "rdc", version, about = "Rossum Deployment as Code")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Bootstrap a new rdc project in the current directory.
    Init {
        #[arg(long)]
        name: String,

        #[arg(long = "env", value_name = "ENV_SPEC", required = true)]
        envs: Vec<String>,
    },
    /// Pull a Rossum environment's configuration into the local snapshot.
    Pull {
        env: String,
    },
    /// Push locally-edited hooks back to the Rossum environment.
    /// (M10: hooks only; other kinds in future milestones.)
    Push {
        env: String,
    },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Command::Init { name, envs }) => crate::cli::init::run(&name, &envs).await,
        Some(Command::Pull { env }) => crate::cli::pull::run(&env).await,
        Some(Command::Push { env }) => crate::cli::push::run(&env).await,
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

pub mod index;
pub mod init;
pub mod pull;
pub mod push;
```

- [ ] **Step 4: Make `cli::pull::common` reachable from push module**

In `src/cli/pull/mod.rs`, change:

```rust
mod common;
```

to:

```rust
pub(crate) mod common;
```

- [ ] **Step 5: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: ALL tests pass — no new tests yet, but build must succeed and existing tests must not regress.

NOTE: existing M7 hook tests use the OLD JSON-only hash. After Task 3 (combined hash), M7 fixtures' first-pull-then-edit-then-second-pull tests should still pass because the lockfile is updated to the new combined hash on the first pull. The M7 conflict test should still pass.

- [ ] **Step 6: Commit**

```bash
git add src/cli/
git commit -m "feat(cli): rdc push for hooks (M10 scope)"
```

---

## Task 6: Integration tests for push

**Files:**
- Create: `tests/cli_push.rs`

- [ ] **Step 1: Create `tests/cli_push.rs`**

```rust
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(format!("testdata/fixtures/{name}")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn empty_list() -> serde_json::Value {
    serde_json::json!({ "pagination": { "next": null }, "results": [] })
}

async fn mount_get_only_hooks_org(server: &MockServer, hooks_payload: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hooks_payload))
        .mount(server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(server).await;
    }
}

/// Local hook edited; remote unchanged → push succeeds via PATCH.
#[tokio::test]
async fn push_succeeds_when_local_edited_and_remote_unchanged() {
    let server = MockServer::start().await;
    mount_get_only_hooks_org(&server, fixture("hooks_list.json")).await;

    // PATCH /hooks/1 returns the hook body (echoing).
    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/1"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hook_1.json")))
        .mount(&server).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success();

    // Edit the hook code (.py)
    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    let edited = format!("{original}# local edit\n");
    std::fs::write(&py_path, &edited).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["push", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("Pushed 1 hook"));
}

/// Local hook edited AND remote also drifted → push skips with conflict warning.
#[tokio::test]
async fn push_skips_when_remote_has_drifted() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    mount_get_only_hooks_org(&server1, fixture("hooks_list.json")).await;

    // server2: remote returns a DIFFERENT hooks_list (drift)
    let drifted_hooks = serde_json::json!({
        "pagination": { "total": 2, "next": null, "previous": null },
        "results": [
            {
                "id": 1,
                "url": "https://mock.rossum.app/api/v1/hooks/1",
                "name": "Validator: invoices (REMOTE DRIFT)",
                "type": "function",
                "queues": ["https://mock.rossum.app/api/v1/queues/100"],
                "events": ["annotation_content"],
                "config": { "runtime": "python3.12", "code": "def x(payload):\n    return {}\n" }
            },
            {
                "id": 2,
                "url": "https://mock.rossum.app/api/v1/hooks/2",
                "name": "SFTP import",
                "type": "function",
                "queues": [],
                "events": ["annotation_status"],
                "config": { "runtime": "python3.12", "code": "def import_files():\n    pass\n" }
            }
        ]
    });
    mount_get_only_hooks_org(&server2, drifted_hooks).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server1.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    // First pull from server1
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success();

    // Edit local
    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{original}# local edit\n")).unwrap();

    // Repoint to server2 (remote has drifted)
    let cfg_path = project.path().join("rdc.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    let new_cfg = cfg.replace(&format!("{}/api/v1", server1.uri()), &format!("{}/api/v1", server2.uri()));
    std::fs::write(&cfg_path, new_cfg).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["push", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("0 hooks"))
        .stdout(predicate::str::contains("1 skipped"));
}

/// Push with no local edits → 0 pushed, 0 skipped.
#[tokio::test]
async fn push_with_no_local_edits_is_noop() {
    let server = MockServer::start().await;
    mount_get_only_hooks_org(&server, fixture("hooks_list.json")).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["push", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("0 hooks"));
}
```

- [ ] **Step 2: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests pass — adds 3 new push tests.

- [ ] **Step 3: Commit**

```bash
git add tests/cli_push.rs
git commit -m "test(cli): integration tests for rdc push (success, drift, no-op)"
```

---

## Task 7: README update

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update Status + add Push section**

Update the Status line:
```
**Status:** M10. `rdc push` for hooks (round-trip closed). All other kinds still pull-only — push for queues/schemas/rules/etc. is future work.
```

Add a new "Push" section after "Conflict handling":

```
## Push (M10 — hooks only)

`rdc push <env>` PATCHes locally-edited hooks back to the Rossum API. Each
hook is checked against the lockfile's content_hash:

- No local edits → skipped silently.
- Local edits AND remote unchanged since last pull → PATCH succeeds.
- Local edits AND remote drifted → push aborted for that hook with a warning.
  Run `rdc pull` to fetch the remote, resolve, then push again.

After a successful push, the lockfile is updated with the server's
authoritative response.

**M10 limitations:**
- Hooks only. Queues, schemas, rules, labels, etc. cannot be pushed yet.
- Updates only. New objects (creates) and deletes are not supported.
- Single-phase. No two-phase send for cross-references (not needed for hooks).
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: M10 — rdc push for hooks"
```

---

## Self-Review

**Spec coverage:**
- §6 CLI surface — `rdc push` added (hooks only)
- §7.3 Push lifecycle — partial: no overlay (M11), no two-phase (deferred), no explicit verify step (PATCH response is verification)

**Placeholder scan:** No "TBD"/"TODO" patterns.

**Type consistency:** `hook_combined_hash(&[u8], &Option<String>) -> String` consistent across Tasks 1, 3, 5. `serialize_hook(&Hook) -> Result<(Vec<u8>, Option<String>)>` consistent in Tasks 2, 3, 5. `RossumClient::update_hook(id, &Hook) -> Result<Hook>` consistent.

**Scope check:** 7 tasks. The novel piece is the push driver and the migration to combined-hash for hooks; everything else follows established patterns.

---

## Next milestones

- **M11:** Overlays — env-specific declarative divergence; modifies push to apply overlays before sending.
- **M12:** Mapping wizard, `rdc plan`, `rdc apply` — deploy workflow.
- **M13:** Push for queues/schemas/rules/labels/engines/etc. (extends M10 pattern).
- **M14:** Auxiliary commands (status, diff, auth, repair); cross-ref indexer.
- **M15:** Distribution.
