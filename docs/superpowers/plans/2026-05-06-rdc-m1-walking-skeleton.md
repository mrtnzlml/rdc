# rdc M1 — Walking Skeleton Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce a working `rdc` binary that can `init` a project and `pull` hooks (only hooks, this milestone) from a Rossum environment, writing a slug-named JSON file plus an extracted `.py` code file per hook to disk. End-to-end tested against a mocked Rossum server.

**Architecture:** Single Rust binary using `clap` for CLI, `reqwest`+`tokio` for async HTTP, `serde`+`serde_json` for model and snapshot codec, `wiremock` for integration tests. Layout follows the spec's module decomposition (`api`, `model`, `snapshot`, `slug`, `state`, `cli`) but only the slices needed for M1 are implemented.

**Tech Stack:** Rust 2021 edition, `clap` 4 (derive), `serde` + `serde_json`, `tokio` 1, `reqwest` 0.12, `anyhow`, `thiserror`, `wiremock` (dev-dep), `tempfile` (dev-dep), `assert_cmd` (dev-dep), `pretty_assertions` (dev-dep).

**What this milestone deliberately omits** (deferred to later milestones):
- All object types except hooks (M2 adds queues, schemas, workspaces, rules, labels, engines, workflows, email templates, MDH).
- Three-way merge for subsequent pulls (M3). M1's `pull` always overwrites; subsequent pulls are not idempotent yet.
- `_index.md` indexer (M4).
- `push`, `plan`, `apply`, `map` commands (M5–M8).
- Overlay engine (M6).
- TUI conflict resolver (M4). Conflicts in M1 surface as overwrites-with-warning.
- Distribution / packaging (M10).

**End state of M1, demonstrable manually:**
```
$ cargo install --path .
$ cd /tmp && mkdir testproj && cd testproj
$ rdc init       # interactive wizard
$ rdc pull dev   # pulls hooks from configured remote
$ ls envs/dev/hooks/
validator-invoices.json  validator-invoices.py
sftp-import.json         sftp-import.py
```

---

## File Structure

Files this plan creates or modifies. Keep this map mentally as you implement; refer back when boundaries blur.

| Path | Responsibility |
|---|---|
| `Cargo.toml` | Workspace + dependency manifest |
| `.gitignore` | Ignore `target/`, `secrets/`, `.rdc/cache/` |
| `src/main.rs` | Binary entry: parse CLI, dispatch to `cli` module, print errors |
| `src/lib.rs` | Library root: re-exports the modules used by tests |
| `src/cli/mod.rs` | Top-level `Cli` enum (clap derive), command dispatch |
| `src/cli/init.rs` | `rdc init` implementation |
| `src/cli/pull.rs` | `rdc pull` implementation |
| `src/api/mod.rs` | `RossumClient` struct, HTTP client, list/get methods |
| `src/api/error.rs` | `ApiError` types |
| `src/model/mod.rs` | Re-exports |
| `src/model/hook.rs` | `Hook` struct (forward-compatible via `extra` map) |
| `src/snapshot/mod.rs` | Re-exports |
| `src/snapshot/writer.rs` | Atomic file writes (tempfile + rename) |
| `src/snapshot/hook.rs` | Hook snapshot codec: split JSON + `.py` |
| `src/slug.rs` | Slug derivation from `name` |
| `src/state/mod.rs` | Re-exports |
| `src/state/lockfile.rs` | Versioned lockfile read/write (M1: write-only minimum) |
| `src/config/mod.rs` | `rdc.toml` model and parsing |
| `tests/cli_init.rs` | Integration test for `rdc init` |
| `tests/cli_pull.rs` | Integration test for `rdc pull` against wiremock |
| `testdata/fixtures/hooks_list.json` | Captured Rossum API list response (anonymized) |
| `testdata/fixtures/hook_1.json` | Single hook detail response |
| `testdata/fixtures/hook_2.json` | Single hook detail response |

---

## Task 1: Initialize Cargo project

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/lib.rs`

- [ ] **Step 1: Create the Cargo manifest**

Create `Cargo.toml`:

```toml
[package]
name = "rdc"
version = "0.0.1"
edition = "2021"
description = "Rossum Deployment as Code — CLI for snapshotting and deploying Rossum.ai configurations"
license = "Apache-2.0"

[[bin]]
name = "rdc"
path = "src/main.rs"

[lib]
name = "rdc"
path = "src/lib.rs"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "fs"] }
toml = "0.8"

[dev-dependencies]
assert_cmd = "2"
predicates = "3"
pretty_assertions = "1"
tempfile = "3"
wiremock = "0.6"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "fs", "test-util"] }
```

- [ ] **Step 2: Create stub binary and library entry points**

Create `src/main.rs`:

```rust
fn main() {
    println!("rdc");
}
```

Create `src/lib.rs`:

```rust
// rdc library root. Modules added in subsequent tasks.
```

- [ ] **Step 3: Verify it builds**

Run: `cargo build`
Expected: `Compiling rdc v0.0.1 ... Finished`. No errors.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/
git commit -m "chore: scaffold rdc cargo project"
```

---

## Task 2: Project gitignore

**Files:**
- Modify: `.gitignore`

- [ ] **Step 1: Add Rust + rdc-specific ignores**

The repo already has a `.gitignore` (or none). Append:

```
# Rust
/target
Cargo.lock.bak

# rdc tool-managed paths (per design spec)
/secrets
/.rdc/cache
```

If `.gitignore` does not exist, create it with the above content.

- [ ] **Step 2: Verify git status is clean apart from this file**

Run: `git status`
Expected: only `.gitignore` shown as modified/new.

- [ ] **Step 3: Commit**

```bash
git add .gitignore
git commit -m "chore: ignore target, secrets, and tool cache"
```

---

## Task 3: CLI scaffold with clap

**Files:**
- Create: `src/cli/mod.rs`
- Modify: `src/main.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write a failing test for `--version`**

Create `tests/cli_version.rs`:

```rust
use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn version_flag_prints_version() {
    Command::cargo_bin("rdc")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("rdc 0.0.1"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test cli_version`
Expected: FAIL — current `main.rs` ignores args.

- [ ] **Step 3: Implement the CLI module**

Create `src/cli/mod.rs`:

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
    /// Bootstrap a new rdc project in the current directory
    Init,
    /// Pull a Rossum environment's configuration into the local snapshot
    Pull {
        /// Environment name as defined in rdc.toml (e.g., dev, test, prod)
        env: String,
    },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Command::Init) => crate::cli::init::run().await,
        Some(Command::Pull { env }) => crate::cli::pull::run(&env).await,
        None => {
            // No subcommand: print help and exit 0
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

pub mod init;
pub mod pull;
```

- [ ] **Step 4: Add stub init and pull modules**

Create `src/cli/init.rs`:

```rust
pub async fn run() -> anyhow::Result<()> {
    anyhow::bail!("init not implemented yet")
}
```

Create `src/cli/pull.rs`:

```rust
pub async fn run(_env: &str) -> anyhow::Result<()> {
    anyhow::bail!("pull not implemented yet")
}
```

- [ ] **Step 5: Wire the CLI into main.rs**

Replace `src/main.rs`:

```rust
use clap::Parser;
use rdc::cli::{run, Cli};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
```

- [ ] **Step 6: Expose `cli` from the library**

Replace `src/lib.rs`:

```rust
pub mod cli;
```

- [ ] **Step 7: Run the version test to verify it passes**

Run: `cargo test --test cli_version`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml src/ tests/
git commit -m "feat(cli): add clap scaffold with init and pull subcommands"
```

---

## Task 4: Slug module

**Files:**
- Create: `src/slug.rs`
- Create: `tests/slug.rs` (unit test colocated, but we put it as integration so compile is fast)
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing tests**

Create `src/slug.rs` with this content:

```rust
// Slug derivation. Stable, deterministic, ASCII-safe.
// Conversion rules:
//   - Lowercase the input.
//   - Replace any sequence of non-alphanumeric ASCII characters with a single hyphen.
//   - Strip leading/trailing hyphens.
//   - If the result is empty (input was all non-alphanumeric), use "_unnamed".
//
// Collision handling is the caller's responsibility (see `slugify_unique`).

pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_hyphen = true; // skip leading hyphens
    for ch in input.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_hyphen = false;
        } else if !prev_hyphen {
            out.push('-');
            prev_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "_unnamed".to_string()
    } else {
        out
    }
}

/// Given a base slug and a set of slugs already in use, return a slug that does
/// not collide. Adds `-2`, `-3`, ... as needed.
pub fn slugify_unique(input: &str, used: &std::collections::HashSet<String>) -> String {
    let base = slugify(input);
    if !used.contains(&base) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !used.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn simple_name() {
        assert_eq!(slugify("Validator: Invoices"), "validator-invoices");
    }

    #[test]
    fn collapses_runs_of_punctuation() {
        assert_eq!(slugify("Foo  ---  Bar!!!"), "foo-bar");
    }

    #[test]
    fn lowercases() {
        assert_eq!(slugify("UPPER lower"), "upper-lower");
    }

    #[test]
    fn empty_string() {
        assert_eq!(slugify(""), "_unnamed");
    }

    #[test]
    fn only_punctuation() {
        assert_eq!(slugify("!!! ???"), "_unnamed");
    }

    #[test]
    fn unicode_stripped() {
        // Non-ASCII characters are dropped (treated as non-alphanumeric).
        assert_eq!(slugify("Faktura č. 1"), "faktura-1");
    }

    #[test]
    fn trims_leading_and_trailing_hyphens() {
        assert_eq!(slugify("---hello---"), "hello");
    }

    #[test]
    fn unique_first_use_returns_base() {
        let used = HashSet::new();
        assert_eq!(slugify_unique("My Hook", &used), "my-hook");
    }

    #[test]
    fn unique_collision_appends_2() {
        let mut used = HashSet::new();
        used.insert("my-hook".to_string());
        assert_eq!(slugify_unique("My Hook", &used), "my-hook-2");
    }

    #[test]
    fn unique_collision_finds_next_free() {
        let mut used = HashSet::new();
        used.insert("my-hook".to_string());
        used.insert("my-hook-2".to_string());
        used.insert("my-hook-3".to_string());
        assert_eq!(slugify_unique("My Hook", &used), "my-hook-4");
    }
}
```

- [ ] **Step 2: Expose the module**

Modify `src/lib.rs`:

```rust
pub mod cli;
pub mod slug;
```

- [ ] **Step 3: Run the tests; expect them to pass**

Run: `cargo test slug::tests`
Expected: PASS — all 10 tests green. (We wrote test+impl together because slug rules are tightly coupled and benefit from being designed together; the test list is the spec.)

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/slug.rs
git commit -m "feat(slug): deterministic slug derivation with collision handling"
```

---

## Task 5: Hook model

**Files:**
- Create: `src/model/mod.rs`
- Create: `src/model/hook.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing test for round-trip**

Create `src/model/hook.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Hook {
    pub id: u64,
    pub url: String,
    pub name: String,
    #[serde(rename = "type")]
    pub hook_type: String,
    #[serde(default)]
    pub queues: Vec<String>,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub config: Value,
    /// Any field we don't model explicitly is preserved here for round-trip fidelity.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn round_trip_preserves_unknown_fields() {
        let payload = json!({
            "id": 856489,
            "url": "https://example.rossum.app/api/v1/hooks/856489",
            "name": "Validator: invoices",
            "type": "function",
            "queues": ["https://example.rossum.app/api/v1/queues/2137275"],
            "events": ["annotation_content"],
            "config": { "code": "print('hi')", "runtime": "python3.12" },
            "modified_at": "2026-04-01T10:00:00Z",
            "future_field_we_have_not_modeled": { "nested": [1, 2, 3] }
        });

        let hook: Hook = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(hook.id, 856489);
        assert_eq!(hook.name, "Validator: invoices");
        assert_eq!(hook.hook_type, "function");

        // Re-serialize; unknown fields must survive byte-identically.
        let round_trip = serde_json::to_value(&hook).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn missing_optional_lists_default_to_empty() {
        let payload = json!({
            "id": 1,
            "url": "https://example/api/v1/hooks/1",
            "name": "Minimal",
            "type": "webhook"
        });
        let hook: Hook = serde_json::from_value(payload).unwrap();
        assert!(hook.queues.is_empty());
        assert!(hook.events.is_empty());
    }
}
```

- [ ] **Step 2: Create module file**

Create `src/model/mod.rs`:

```rust
pub mod hook;

pub use hook::Hook;
```

- [ ] **Step 3: Expose the module**

Modify `src/lib.rs`:

```rust
pub mod cli;
pub mod model;
pub mod slug;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test model::hook`
Expected: PASS — both tests green.

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat(model): Hook struct with forward-compatible extra fields"
```

---

## Task 6: Snapshot writer (atomic file write helper)

**Files:**
- Create: `src/snapshot/mod.rs`
- Create: `src/snapshot/writer.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write a failing test for atomic write**

Create `src/snapshot/writer.rs`:

```rust
use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::Path;

/// Write `bytes` to `path` atomically: write to a sibling temp file, then rename.
/// Creates parent directories if missing.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("creating temp file {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing temp file {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing temp file {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn writes_bytes_to_new_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a/b/c.txt");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.txt");
        write_atomic(&path, b"v1").unwrap();
        write_atomic(&path, b"v2").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"v2");
    }

    #[test]
    fn temp_file_does_not_persist_on_success() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("y.txt");
        write_atomic(&path, b"data").unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec!["y.txt"]);
    }
}
```

- [ ] **Step 2: Create module file**

Create `src/snapshot/mod.rs`:

```rust
pub mod writer;
```

- [ ] **Step 3: Expose the module**

Modify `src/lib.rs`:

```rust
pub mod cli;
pub mod model;
pub mod slug;
pub mod snapshot;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test snapshot::writer::tests`
Expected: PASS — all 3 tests green.

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat(snapshot): atomic file write helper"
```

---

## Task 7: Hook snapshot codec

**Files:**
- Create: `src/snapshot/hook.rs`
- Modify: `src/snapshot/mod.rs`

- [ ] **Step 1: Write failing tests**

Create `src/snapshot/hook.rs`:

```rust
use crate::model::Hook;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

/// Write a hook to disk: a JSON file under `<dir>/<slug>.json` and, if the hook
/// has Python code, a sibling `<slug>.py` file. The `code` field of `config` is
/// stripped from the JSON to avoid duplication; the `.py` file becomes the
/// source of truth.
///
/// Returns the JSON path written.
pub fn write_hook(dir: &Path, slug: &str, hook: &Hook) -> Result<()> {
    let mut json_value = serde_json::to_value(hook)
        .context("serializing hook to value")?;

    // Extract `config.code` into a sibling .py file.
    let code = json_value
        .get_mut("config")
        .and_then(|c| c.as_object_mut())
        .and_then(|m| m.remove("code"))
        .and_then(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        });

    let json_path = dir.join(format!("{slug}.json"));
    let json_bytes = serde_json::to_vec_pretty(&json_value)
        .context("serializing hook json")?;
    let mut json_with_newline = json_bytes;
    json_with_newline.push(b'\n');
    write_atomic(&json_path, &json_with_newline)?;

    if let Some(code) = code {
        let py_path = dir.join(format!("{slug}.py"));
        let mut bytes = code.into_bytes();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        write_atomic(&py_path, &bytes)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Hook;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample_hook() -> Hook {
        let v = json!({
            "id": 1,
            "url": "https://example/api/v1/hooks/1",
            "name": "Sample",
            "type": "function",
            "queues": [],
            "events": [],
            "config": { "runtime": "python3.12", "code": "def x():\n    return 1\n" }
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn writes_json_and_py() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &sample_hook()).unwrap();
        assert!(dir.path().join("sample.json").exists());
        assert!(dir.path().join("sample.py").exists());
    }

    #[test]
    fn json_does_not_contain_code_field() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &sample_hook()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("sample.json")).unwrap();
        assert!(!raw.contains("def x"), "code should be in .py, not .json");
        assert!(raw.contains("python3.12"), "other config preserved");
    }

    #[test]
    fn py_contains_code_with_trailing_newline() {
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &sample_hook()).unwrap();
        let py = std::fs::read_to_string(dir.path().join("sample.py")).unwrap();
        assert_eq!(py, "def x():\n    return 1\n");
    }

    #[test]
    fn no_py_file_when_hook_has_no_code() {
        let mut hook = sample_hook();
        // Remove the code field
        if let Value::Object(map) = &mut hook.config {
            map.remove("code");
        }
        let dir = TempDir::new().unwrap();
        write_hook(dir.path(), "sample", &hook).unwrap();
        assert!(dir.path().join("sample.json").exists());
        assert!(!dir.path().join("sample.py").exists());
    }
}
```

- [ ] **Step 2: Wire into snapshot module**

Modify `src/snapshot/mod.rs`:

```rust
pub mod hook;
pub mod writer;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test snapshot::hook::tests`
Expected: PASS — all 4 tests green.

- [ ] **Step 4: Commit**

```bash
git add src/
git commit -m "feat(snapshot): hook codec writes JSON + extracted .py"
```

---

## Task 8: API error type

**Files:**
- Create: `src/api/mod.rs`
- Create: `src/api/error.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Define error types**

Create `src/api/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Rossum API returned status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("response body could not be decoded as JSON: {0}")]
    Decode(#[from] serde_json::Error),
}
```

- [ ] **Step 2: Stub the api module**

Create `src/api/mod.rs`:

```rust
pub mod error;

pub use error::ApiError;
```

- [ ] **Step 3: Expose**

Modify `src/lib.rs`:

```rust
pub mod api;
pub mod cli;
pub mod model;
pub mod slug;
pub mod snapshot;
```

- [ ] **Step 4: Verify compile**

Run: `cargo build`
Expected: clean build, no warnings related to new files.

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat(api): error type"
```

---

## Task 9: API client — list and get hooks

**Files:**
- Modify: `src/api/mod.rs`
- Create: `tests/api_hooks.rs`
- Create: `testdata/fixtures/hooks_list.json`
- Create: `testdata/fixtures/hook_1.json`
- Create: `testdata/fixtures/hook_2.json`

- [ ] **Step 1: Create fixture files**

Create `testdata/fixtures/hooks_list.json`:

```json
{
  "pagination": {
    "total": 2,
    "total_pages": 1,
    "next": null,
    "previous": null
  },
  "results": [
    {
      "id": 1,
      "url": "https://mock.rossum.app/api/v1/hooks/1",
      "name": "Validator: invoices",
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
}
```

Create `testdata/fixtures/hook_1.json`: copy the first object from `results` above (same JSON object as a top-level value).

Create `testdata/fixtures/hook_2.json`: copy the second object similarly.

- [ ] **Step 2: Write the failing integration test**

Create `tests/api_hooks.rs`:

```rust
use rdc::api::RossumClient;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(format!("testdata/fixtures/{name}")).unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[tokio::test]
async fn list_hooks_paginates_until_done() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "TEST_TOKEN".into());
    let hooks = client.list_hooks().await.unwrap();
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0].name, "Validator: invoices");
    assert_eq!(hooks[1].name, "SFTP import");
}

#[tokio::test]
async fn auth_failure_surfaces_status_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let client = RossumClient::new(format!("{}/api/v1", server.uri()), "BAD".into());
    let err = client.list_hooks().await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "error should mention 401, got: {msg}");
}
```

- [ ] **Step 3: Run the test to confirm it fails**

Run: `cargo test --test api_hooks`
Expected: FAIL — `RossumClient` and `list_hooks` not defined.

- [ ] **Step 4: Implement the client**

Replace `src/api/mod.rs`:

```rust
pub mod error;

pub use error::ApiError;

use crate::model::Hook;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

/// Rossum API client. Holds a base URL (e.g. `https://X.rossum.app/api/v1`)
/// and a static API token. M1 only implements the methods needed for `pull`
/// of hooks. Pagination is followed transparently.
pub struct RossumClient {
    base_url: String,
    token: String,
    http: Client,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    pagination: Pagination,
    results: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct Pagination {
    next: Option<String>,
}

impl RossumClient {
    pub fn new(base_url: String, token: String) -> Self {
        let http = Client::builder()
            .build()
            .expect("reqwest client builder cannot fail with default config");
        Self { base_url, token, http }
    }

    pub async fn list_hooks(&self) -> Result<Vec<Hook>> {
        let mut url = format!("{}/hooks", self.base_url);
        let mut out = Vec::new();
        loop {
            let page: Page<Hook> = self.get_json(&url).await?;
            out.extend(page.results);
            match page.pagination.next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self
            .http
            .get(url)
            .header("Authorization", format!("token {}", self.token))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status {
                status: status.as_u16(),
                body,
            }
            .into());
        }
        let value = resp
            .json::<T>()
            .await
            .with_context(|| format!("decoding response from {url}"))?;
        Ok(value)
    }
}
```

- [ ] **Step 5: Run the tests, expect green**

Run: `cargo test --test api_hooks`
Expected: PASS — both tests green.

- [ ] **Step 6: Commit**

```bash
git add src/ tests/ testdata/
git commit -m "feat(api): RossumClient with list_hooks and pagination"
```

---

## Task 10: Project config (rdc.toml)

**Files:**
- Create: `src/config/mod.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing tests**

Create `src/config/mod.rs`:

```rust
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ProjectConfig {
    pub project: ProjectMeta,
    #[serde(default)]
    pub envs: BTreeMap<String, EnvConfig>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ProjectMeta {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct EnvConfig {
    pub api_base: String,
    pub org_id: u64,
    #[serde(default)]
    pub workspace_filter: Option<String>,
}

impl ProjectConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: ProjectConfig = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)
            .context("serializing project config")?;
        crate::snapshot::writer::write_atomic(path, s.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn example() -> ProjectConfig {
        let mut envs = BTreeMap::new();
        envs.insert(
            "dev".to_string(),
            EnvConfig {
                api_base: "https://example.rossum.app/api/v1".to_string(),
                org_id: 285704,
                workspace_filter: None,
            },
        );
        ProjectConfig {
            project: ProjectMeta { name: "demo".to_string() },
            envs,
        }
    }

    #[test]
    fn round_trip_to_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rdc.toml");
        example().save(&path).unwrap();
        let loaded = ProjectConfig::load(&path).unwrap();
        assert_eq!(loaded, example());
    }

    #[test]
    fn missing_file_errors_with_path() {
        let err = ProjectConfig::load(Path::new("/nope/rdc.toml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("/nope/rdc.toml"), "error should name the path: {msg}");
    }
}
```

- [ ] **Step 2: Expose**

Modify `src/lib.rs`:

```rust
pub mod api;
pub mod cli;
pub mod config;
pub mod model;
pub mod slug;
pub mod snapshot;
```

- [ ] **Step 3: Run tests**

Run: `cargo test config::tests`
Expected: PASS — both tests green.

- [ ] **Step 4: Commit**

```bash
git add src/
git commit -m "feat(config): ProjectConfig load/save with envs"
```

---

## Task 11: Lockfile (minimum viable)

**Files:**
- Create: `src/state/mod.rs`
- Create: `src/state/lockfile.rs`
- Modify: `src/lib.rs`

For M1 the lockfile only needs to record per-object slug↔ID mapping plus the `modified_at` we last saw. Three-way merge support comes in M3.

- [ ] **Step 1: Write failing tests**

Create `src/state/lockfile.rs`:

```rust
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const LOCKFILE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Lockfile {
    pub version: u32,
    /// Per object-type, a map of slug -> entry.
    pub objects: BTreeMap<String, BTreeMap<String, ObjectEntry>>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ObjectEntry {
    pub id: u64,
    /// ISO 8601 timestamp from the server (`modified_at`), if present.
    #[serde(default)]
    pub modified_at: Option<String>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self { version: LOCKFILE_VERSION, objects: BTreeMap::new() }
    }
}

impl Lockfile {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let lf: Lockfile = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        if lf.version != LOCKFILE_VERSION {
            anyhow::bail!(
                "lockfile {} has version {} but this rdc supports {}",
                path.display(),
                lf.version,
                LOCKFILE_VERSION
            );
        }
        Ok(lf)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = serde_json::to_string_pretty(self)
            .context("serializing lockfile")?;
        crate::snapshot::writer::write_atomic(path, format!("{s}\n").as_bytes())?;
        Ok(())
    }

    pub fn upsert(&mut self, kind: &str, slug: &str, entry: ObjectEntry) {
        self.objects
            .entry(kind.to_string())
            .or_default()
            .insert(slug.to_string(), entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_returns_empty() {
        let lf = Lockfile::load(Path::new("/nope.json")).unwrap();
        assert_eq!(lf, Lockfile::default());
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        let mut lf = Lockfile::default();
        lf.upsert("hooks", "validator-invoices", ObjectEntry {
            id: 1,
            modified_at: Some("2026-04-01T10:00:00Z".to_string()),
        });
        lf.save(&path).unwrap();
        let loaded = Lockfile::load(&path).unwrap();
        assert_eq!(loaded, lf);
    }

    #[test]
    fn future_version_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dev.lock.json");
        std::fs::write(&path, r#"{"version":999,"objects":{}}"#).unwrap();
        let err = Lockfile::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("version"));
    }
}
```

- [ ] **Step 2: Module wiring**

Create `src/state/mod.rs`:

```rust
pub mod lockfile;

pub use lockfile::{Lockfile, ObjectEntry, LOCKFILE_VERSION};
```

Modify `src/lib.rs`:

```rust
pub mod api;
pub mod cli;
pub mod config;
pub mod model;
pub mod slug;
pub mod snapshot;
pub mod state;
```

- [ ] **Step 3: Run tests**

Run: `cargo test state::lockfile::tests`
Expected: PASS — all 3 tests green.

- [ ] **Step 4: Commit**

```bash
git add src/
git commit -m "feat(state): versioned lockfile with atomic save"
```

---

## Task 12: `rdc init` implementation

**Files:**
- Modify: `src/cli/init.rs`
- Create: `tests/cli_init.rs`

`init` writes `rdc.toml` with one example env, creates `envs/<env>/` skeletons, and writes a `.gitignore` if absent. M1's init is non-interactive: it accepts env vars/flags or uses defaults. (Interactive wizard can come in M9.)

- [ ] **Step 1: Write failing integration test**

Create `tests/cli_init.rs`:

```rust
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn init_creates_expected_files() {
    let dir = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--name", "demo",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized"));

    assert!(dir.path().join("rdc.toml").exists());
    assert!(dir.path().join(".gitignore").exists());
    assert!(dir.path().join("envs/dev").is_dir());
    assert!(dir.path().join("secrets").is_dir());

    let cfg = std::fs::read_to_string(dir.path().join("rdc.toml")).unwrap();
    assert!(cfg.contains("name = \"demo\""));
    assert!(cfg.contains("[envs.dev]"));
    assert!(cfg.contains("api_base = \"https://example.rossum.app/api/v1\""));

    let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains("/secrets"));
    assert!(gitignore.contains("/.rdc/cache"));
}

#[test]
fn init_refuses_to_clobber_existing_project() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("rdc.toml"), "stub").unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "init",
            "--name", "demo",
            "--env", "dev=https://example.rossum.app/api/v1:285704",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already initialized"));
}
```

- [ ] **Step 2: Add the `init` flags to the CLI enum**

Modify `src/cli/mod.rs`. Replace the `Command` enum and its dispatch:

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
        /// Project name (written into rdc.toml).
        #[arg(long)]
        name: String,

        /// One or more env definitions of the form `<env>=<api_base>:<org_id>`.
        /// Example: `--env dev=https://demo.rossum.app/api/v1:285704`
        #[arg(long = "env", value_name = "ENV_SPEC", required = true)]
        envs: Vec<String>,
    },
    /// Pull a Rossum environment's configuration into the local snapshot.
    Pull {
        /// Environment name as defined in rdc.toml.
        env: String,
    },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Command::Init { name, envs }) => crate::cli::init::run(&name, &envs).await,
        Some(Command::Pull { env }) => crate::cli::pull::run(&env).await,
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

pub mod init;
pub mod pull;
```

- [ ] **Step 3: Implement `init`**

Replace `src/cli/init.rs`:

```rust
use crate::config::{EnvConfig, ProjectConfig, ProjectMeta};
use crate::snapshot::writer::write_atomic;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

pub async fn run(name: &str, env_specs: &[String]) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    if cfg_path.exists() {
        return Err(anyhow!(
            "directory is already initialized as an rdc project (rdc.toml exists at {})",
            cfg_path.display()
        ));
    }

    let mut envs = BTreeMap::new();
    for spec in env_specs {
        let (env_name, env_cfg) = parse_env_spec(spec)?;
        envs.insert(env_name, env_cfg);
    }

    let cfg = ProjectConfig {
        project: ProjectMeta { name: name.to_string() },
        envs: envs.clone(),
    };
    cfg.save(&cfg_path)?;

    write_gitignore(&cwd)?;
    std::fs::create_dir_all(cwd.join("secrets"))
        .with_context(|| format!("creating {}", cwd.join("secrets").display()))?;
    for env in envs.keys() {
        let env_dir = cwd.join("envs").join(env);
        std::fs::create_dir_all(&env_dir)
            .with_context(|| format!("creating {}", env_dir.display()))?;
        std::fs::create_dir_all(env_dir.join("hooks"))
            .with_context(|| format!("creating {}", env_dir.join("hooks").display()))?;
    }

    println!(
        "Initialized rdc project '{name}' with envs: {}",
        envs.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

fn parse_env_spec(spec: &str) -> Result<(String, EnvConfig)> {
    let (env_name, rest) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid --env spec '{spec}': expected `<env>=<api_base>:<org_id>`"))?;
    let last_colon = rest
        .rfind(':')
        .ok_or_else(|| anyhow!("invalid --env spec '{spec}': missing :<org_id>"))?;
    let api_base = &rest[..last_colon];
    let org_id_str = &rest[last_colon + 1..];
    let org_id: u64 = org_id_str
        .parse()
        .with_context(|| format!("parsing org_id '{org_id_str}' in spec '{spec}'"))?;
    Ok((
        env_name.to_string(),
        EnvConfig {
            api_base: api_base.to_string(),
            org_id,
            workspace_filter: None,
        },
    ))
}

fn write_gitignore(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let body = "/target\n/secrets\n/.rdc/cache\n";
    if path.exists() {
        let existing = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if existing.contains("/secrets") && existing.contains("/.rdc/cache") {
            return Ok(());
        }
        let mut combined = existing;
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(body);
        write_atomic(&path, combined.as_bytes())?;
    } else {
        write_atomic(&path, body.as_bytes())?;
    }
    Ok(())
}
```

- [ ] **Step 4: Run the integration test**

Run: `cargo test --test cli_init`
Expected: PASS — both tests green.

- [ ] **Step 5: Run the full unit suite to ensure nothing regressed**

Run: `cargo test`
Expected: all tests pass (slug, model, snapshot, state, config, init).

- [ ] **Step 6: Commit**

```bash
git add src/ tests/
git commit -m "feat(cli): rdc init scaffolds project files"
```

---

## Task 13: Secrets loader

**Files:**
- Create: `src/secrets.rs`
- Modify: `src/lib.rs`

The pull command needs the API token. M1 supports two sources: env var `RDC_TOKEN_<ENV>` (uppercase) and `secrets/<env>.secrets.json`. The env var wins.

- [ ] **Step 1: Write failing tests**

Create `src/secrets.rs`:

```rust
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Resolve the API token for an environment.
///
/// Resolution order:
/// 1. `RDC_TOKEN_<UPPER_ENV>` environment variable.
/// 2. `secrets/<env>.secrets.json` with shape `{ "api_token": "..." }`.
///
/// Returns an actionable error if neither source is present.
pub fn resolve_token(project_root: &Path, env: &str) -> Result<String> {
    let env_var = format!("RDC_TOKEN_{}", env.to_uppercase());
    if let Ok(t) = std::env::var(&env_var) {
        if !t.is_empty() {
            return Ok(t);
        }
    }

    let path = project_root.join("secrets").join(format!("{env}.secrets.json"));
    if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        #[derive(Deserialize)]
        struct File {
            api_token: String,
        }
        let f: File = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        if f.api_token.is_empty() {
            return Err(anyhow!(
                "{} has empty api_token; set ${env_var} or fill in the file",
                path.display()
            ));
        }
        return Ok(f.api_token);
    }

    Err(anyhow!(
        "no token for env '{env}': set ${env_var} or write {}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn env_var_wins() {
        let dir = TempDir::new().unwrap();
        // SAFETY: env vars are process-global; tests run in parallel by default
        // but each uses a unique env name to avoid collisions.
        std::env::set_var("RDC_TOKEN_UNITTEST_A", "from-env");
        let token = resolve_token(dir.path(), "unittest_a").unwrap();
        assert_eq!(token, "from-env");
        std::env::remove_var("RDC_TOKEN_UNITTEST_A");
    }

    #[test]
    fn file_used_when_env_var_absent() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(
            dir.path().join("secrets/unittest_b.secrets.json"),
            r#"{"api_token":"from-file"}"#,
        )
        .unwrap();
        let token = resolve_token(dir.path(), "unittest_b").unwrap();
        assert_eq!(token, "from-file");
    }

    #[test]
    fn missing_token_errors_with_actionable_message() {
        let dir = TempDir::new().unwrap();
        let err = resolve_token(dir.path(), "unittest_c").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("RDC_TOKEN_UNITTEST_C"), "should mention env var: {msg}");
        assert!(msg.contains("secrets/unittest_c.secrets.json"), "should mention file path: {msg}");
    }
}
```

- [ ] **Step 2: Expose**

Modify `src/lib.rs`:

```rust
pub mod api;
pub mod cli;
pub mod config;
pub mod model;
pub mod secrets;
pub mod slug;
pub mod snapshot;
pub mod state;
```

- [ ] **Step 3: Run tests**

Run: `cargo test secrets::tests`
Expected: PASS — all 3 tests green.

- [ ] **Step 4: Commit**

```bash
git add src/
git commit -m "feat(secrets): token resolver (env var or secrets file)"
```

---

## Task 14: `rdc pull` implementation

**Files:**
- Modify: `src/cli/pull.rs`
- Create: `tests/cli_pull.rs`

- [ ] **Step 1: Write failing integration test**

Create `tests/cli_pull.rs`:

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

#[tokio::test]
async fn pull_writes_hook_json_and_py_files() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .and(header("Authorization", "token TEST_TOKEN"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server)
        .await;

    let project = TempDir::new().unwrap();

    // Bootstrap project with a single env pointing at the mock server.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--name", "test-pull",
            "--env",
            &format!("dev={}/api/v1:1", server.uri()),
        ])
        .assert()
        .success();

    // Drop a token file (env-var path is exercised in unit tests).
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Pulled 2 hooks"));

    let hooks_dir = project.path().join("envs/dev/hooks");
    assert!(hooks_dir.join("validator-invoices.json").exists());
    assert!(hooks_dir.join("validator-invoices.py").exists());
    assert!(hooks_dir.join("sftp-import.json").exists());
    assert!(hooks_dir.join("sftp-import.py").exists());

    let py = std::fs::read_to_string(hooks_dir.join("validator-invoices.py")).unwrap();
    assert!(py.contains("def x"));

    // Lockfile recorded both objects.
    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf.contains("validator-invoices"));
    assert!(lf.contains("sftp-import"));
}

#[tokio::test]
async fn pull_with_missing_token_fails_with_helpful_error() {
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--name", "x",
            "--env", "dev=https://nope.invalid/api/v1:1",
        ])
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("RDC_TOKEN_DEV"));
}

#[tokio::test]
async fn pull_with_unknown_env_fails() {
    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args([
            "init",
            "--name", "x",
            "--env", "dev=https://nope.invalid/api/v1:1",
        ])
        .assert()
        .success();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "prod"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("env 'prod' is not defined"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test cli_pull`
Expected: FAIL — pull is still the stub from Task 3.

- [ ] **Step 3: Implement pull**

Replace `src/cli/pull.rs`:

```rust
use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::secrets::resolve_token;
use crate::slug::slugify_unique;
use crate::snapshot::hook::write_hook;
use crate::state::{Lockfile, ObjectEntry};
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;

pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)
        .with_context(|| format!("loading project config from {}", cfg_path.display()))?;

    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token);

    let hooks = client
        .list_hooks()
        .await
        .with_context(|| format!("listing hooks for env '{env}'"))?;

    let env_root = cwd.join("envs").join(env);
    let hooks_dir = env_root.join("hooks");
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("creating {}", hooks_dir.display()))?;

    let lockfile_path = cwd
        .join(".rdc")
        .join("state")
        .join(format!("{env}.lock.json"));
    let mut lockfile = Lockfile::load(&lockfile_path)?;

    let mut used_slugs: HashSet<String> = HashSet::new();
    for hook in &hooks {
        let slug = slugify_unique(&hook.name, &used_slugs);
        used_slugs.insert(slug.clone());

        write_hook(&hooks_dir, &slug, hook)
            .with_context(|| format!("writing hook '{}' to disk", hook.name))?;

        lockfile.upsert(
            "hooks",
            &slug,
            ObjectEntry {
                id: hook.id,
                modified_at: hook
                    .extra
                    .get("modified_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            },
        );
    }

    lockfile.save(&lockfile_path)?;

    println!("Pulled {} hooks from env '{env}'", hooks.len());
    Ok(())
}
```

- [ ] **Step 4: Run integration tests**

Run: `cargo test --test cli_pull`
Expected: all 3 tests PASS.

- [ ] **Step 5: Run the full test suite**

Run: `cargo test`
Expected: all tests across all modules pass.

- [ ] **Step 6: Commit**

```bash
git add src/ tests/
git commit -m "feat(cli): rdc pull writes hooks to per-env snapshot + lockfile"
```

---

## Task 15: End-to-end manual smoke (documentation)

**Files:**
- Create: `README.md`

The skill discourages writing docs for their own sake, but a minimal README is necessary for someone to know how to invoke the binary. Keep it short.

- [ ] **Step 1: Write README**

Create `README.md`:

```markdown
# rdc

`rdc` (Rossum Deployment as Code) snapshots Rossum.ai configurations to disk for AI-assisted local development and deploys them across environments.

**Status:** M1 (walking skeleton). Only `rdc init` and `rdc pull <env>` for hooks are implemented. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design and `docs/superpowers/plans/` for implementation plans.

## Quick start (M1)

```sh
cargo install --path .

mkdir my-rossum-project && cd my-rossum-project
rdc init --name my-project \
  --env dev=https://YOUR-ORG.rossum.app/api/v1:YOUR_ORG_ID

# Provide a token for the dev env:
echo '{"api_token":"YOUR_TOKEN"}' > secrets/dev.secrets.json
# OR: export RDC_TOKEN_DEV=YOUR_TOKEN

rdc pull dev
ls envs/dev/hooks/
```

## Tests

```sh
cargo test
```
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: add minimal README for M1"
```

---

## Self-Review

**Spec coverage check:**

| Spec section | Covered by |
|---|---|
| §5.1 Workspace layout | Tasks 12 (init), 14 (pull writes hooks under `envs/<env>/hooks/`) |
| §5.2 Modules — `api`, `model`, `snapshot`, `slug`, `state`, `config`, `secrets`, `cli` | Tasks 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14 |
| §5.2 Modules — `overlay`, `diff`, `merge`, `resolve`, `map`, `plan`, `indexer` | Deferred to M3–M8 (called out in milestone summary) |
| §6 CLI surface — `init` and `pull` | Tasks 12, 14 |
| §6 CLI surface — `push`, `plan`, `apply`, `map`, `diff`, `status`, `auth`, `repair` | Deferred to M5+ |
| §7.2 pull data flow steps 1–3, 8–10 (auth, fetch, decode, write, lockfile, summary) | Tasks 13, 9, 5, 7, 11, 14 |
| §7.2 pull steps 4–7 (three-way compare, merge, conflict resolver, overlay reverse) | Deferred to M3–M6; M1 always overwrites |
| §11 Object scope | Hooks only in M1; everything else in M2 |
| §12 Authentication and secrets | Task 13 (env var + secrets file). Hook secrets deferred until `push` (M5) |
| §13 Error handling — actionable messages, atomic writes | Tasks 6 (atomic), 9 (status errors), 13 (token errors) |
| §14 Testing — unit, integration, pull-twice-zero-diff | Unit tests every task; integration in 9, 12, 14. Zero-diff property test deferred to M3 (needs merge layer) |

**Placeholder scan:** Reviewed for "TBD", "TODO", "fill in", "similar to" patterns — none present. Every step shows actual code or actual commands.

**Type consistency:**
- `Hook` struct used in `model/hook.rs` (Task 5), `snapshot/hook.rs` (Task 7), `api/mod.rs` (Task 9), `cli/pull.rs` (Task 14). Field names consistent.
- `Lockfile`, `ObjectEntry` used in Task 11, Task 14. Field names consistent.
- `ProjectConfig`, `EnvConfig` used in Task 10, Task 12, Task 14. Consistent.
- `RossumClient::new(base_url, token)` signature consistent across Task 9 and Task 14.
- `slugify_unique(name, &used)` signature matches in Task 4 and Task 14.
- `write_atomic(path, bytes)` matches in Task 6, Task 7, Task 10, Task 11.

**Scope check:** This plan produces one shippable, testable unit (`rdc init` + `rdc pull <env>` for hooks). It is not too large for a single implementation pass.

---

## Next milestones

Each subsequent milestone gets its own plan document under `docs/superpowers/plans/`. Brief outline:

- **M2:** Extend snapshot codec to all object types (queues, schemas, workspaces, rules, labels, engines, engine fields, workflows, workflow steps, email templates, MDH dataset metadata + indexes). Extend `RossumClient` with corresponding list/get methods.
- **M3:** Three-way merge layer. Lockfile snapshots base state per object. `pull` becomes idempotent. Pull-twice-zero-diff property test.
- **M4:** Indexer (`_index.md` per env directory) and TUI conflict resolver.
- **M5:** `rdc push` with pre-push fetch, three-way merge, two-phase send, hook-secrets upload.
- **M6:** Overlay engine; integrated into pull and push.
- **M7:** `rdc map` wizard; mapping file format.
- **M8:** `rdc plan` and `rdc apply` deploy commands.
- **M9:** Auxiliary commands — `status`, `diff`, `auth`, `repair`. Interactive `init` wizard variant.
- **M10:** Distribution — Homebrew tap, GitHub releases (cross-compiled binaries), `curl | sh` installer, `rdc update` self-update.
