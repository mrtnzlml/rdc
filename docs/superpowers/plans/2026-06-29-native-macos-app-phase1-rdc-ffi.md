# Native macOS App — Phase 1: `rdc-ffi` foundation + repo restructure

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Tauri app's Rust backend with a standalone `rdc-ffi` crate that exposes rdc's embed surface to Swift via UniFFI, remove the Tauri/React app, and produce a buildable `.xcframework` + Swift bindings — leaving a tested foundation the SwiftUI app (Phase 2) consumes.

**Architecture:** A new workspace member `rdc-ffi/` wraps the existing `rdc` crate. It re-exposes five operations (`list_connections`, `add_connection`, `edit_credentials`, `validate_existing_project`, `sync_connection`) over a UniFFI C-ABI surface, plus a current-thread Tokio runtime wrapper so the `async`/`!Send` sync engine runs correctly behind a synchronous-from-Swift call. The on-disk contract (`rdc.toml`, `secrets/main.secrets.json`, `envs/main`, `.rdc/state`) is untouched — all reads/writes route through existing `rdc::secrets`/`rdc::slug`/`rdc::cli` functions, never reimplemented.

**Tech Stack:** Rust (edition 2024), UniFFI (proc-macro mode), Tokio (current-thread runtime), the existing `rdc` crate. Swift binding generation via `uniffi-bindgen`; universal static lib via `lipo` + `xcodebuild -create-xcframework`.

## Global Constraints

- **Edition:** `2024` (`edition.workspace = true`) — copy verbatim from existing members.
- **License:** `WTFPL` (`license.workspace = true`).
- **Dead code is a hard error:** the workspace lint `dead_code = "deny"` applies. Every helper must be reachable from an exported function or a non-`#[cfg(test)]` path — `#[cfg(test)]`-only usage does NOT satisfy the lint.
- **Release profile is workspace-global:** `panic = "abort"`, `lto = "fat"`, `strip = true`. A member cannot override it. Consequence: a panic inside the Rust core aborts the app rather than surfacing as a catchable error — this matches the existing CLI and Tauri-app posture and is accepted for v1.
- **Single source of truth:** never parse or write `rdc.toml` / `secrets/*.secrets.json` formats by hand in new code where an `rdc::` function exists. Credentials always go through `rdc::secrets::*`.
- **Single env only:** every project uses env name `"main"` (literal). This is a fixed convention, not a customer identifier.
- **Customer confidentiality:** no customer names or customer-specific identifiers (org/division/region codes, environment names, queue/engine/hook slugs, hostnames, URLs, paths) anywhere — code, tests, docs, commit messages. Use neutral placeholders (`acme`, `main`, `invoices`, `https://example.test/api/v1`).
- **Identity (for Phase 2, recorded here for continuity):** bundle identifier `ai.rossum.local`, product name "Rossum Local", macOS 13.0 floor.

---

## File Structure

| File | Responsibility |
|---|---|
| `Cargo.toml` (root, modify) | Swap workspace member `desktop` → `rdc-ffi` |
| `desktop/` (delete) | Remove the Tauri Rust + React/Vite app |
| `assets/icons/` (move) | Preserve `desktop/icons/*` for the Phase 2 app |
| `README.md` (modify) | Update the "Desktop app (macOS)" section |
| `rdc-ffi/Cargo.toml` (create) | Crate manifest, crate-types, uniffi-bindgen bin |
| `rdc-ffi/src/lib.rs` (create) | `setup_scaffolding!`, module wiring, `ffi_version` |
| `rdc-ffi/src/bin/uniffi-bindgen.rs` (create) | Binding generator entry point |
| `rdc-ffi/src/error.rs` (create) | `FfiError` + `op`/`map_err` helpers |
| `rdc-ffi/src/discover.rs` (create) | Connection discovery (ported from `desktop/src/discover.rs`) |
| `rdc-ffi/src/connections.rs` (create) | DTOs + `list/add/edit/validate` exported fns |
| `rdc-ffi/src/sync.rs` (create) | Runtime wrapper + `sync_connection` + progress callback |
| `rdc-ffi/build-xcframework.sh` (create) | Build universal lib + generate Swift bindings + xcframework |
| `rdc-ffi/.gitignore` (create) | Ignore generated bindings + xcframework artifacts |
| `rdc-ffi/README.md` (create) | How to build the FFI artifact |

---

## Task 1: Restructure workspace — remove Tauri app, preserve icons, scaffold empty `rdc-ffi`

**Files:**
- Modify: `Cargo.toml` (root) — `[workspace] members`
- Delete: `desktop/` (entire directory)
- Create: `assets/icons/icon.icns`, `assets/icons/icon.png`, `assets/icons/gen-icon.py` (moved from `desktop/icons/`)
- Create: `rdc-ffi/Cargo.toml`, `rdc-ffi/src/lib.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes: nothing (first task).
- Produces: a workspace with members `[".", "rdc-ffi"]` that builds; `rdc_ffi::version() -> Option<&'static str>`.

- [ ] **Step 1: Confirm nothing outside `desktop/` references the old crate**

Run: `grep -rn "rossum_local\|rossum-local" --include="*.rs" --include="*.toml" . | grep -v "^\./desktop/" | grep -v "^\./target/"`
Expected: no output (empty). If anything prints, stop and resolve before deleting.

- [ ] **Step 2: Preserve the app icon assets, then remove the Tauri app**

```bash
mkdir -p assets/icons
git mv desktop/icons/icon.icns   assets/icons/icon.icns
git mv desktop/icons/icon.png    assets/icons/icon.png
git mv desktop/icons/gen-icon.py assets/icons/gen-icon.py
git rm -r desktop
```
Expected: `desktop/` removed from the index; three icon files staged under `assets/icons/`.

- [ ] **Step 3: Point the workspace at `rdc-ffi` instead of `desktop`**

In `Cargo.toml` (root), change:
```toml
[workspace]
members = [".", "desktop"]
```
to:
```toml
[workspace]
members = [".", "rdc-ffi"]
```

- [ ] **Step 4: Create the minimal `rdc-ffi` crate manifest**

Create `rdc-ffi/Cargo.toml`:
```toml
[package]
name = "rdc-ffi"
version = "0.1.0"
edition.workspace = true
license.workspace = true
description = "FFI bridge exposing rdc's embed surface to the native macOS app"

[lints]
workspace = true

[lib]
name = "rdc_ffi"
crate-type = ["staticlib", "cdylib", "lib"]

[dependencies]
rdc = { path = ".." }

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 5: Create the minimal lib so the workspace compiles**

Create `rdc-ffi/src/lib.rs`:
```rust
//! FFI bridge from the native macOS app ("Rossum Local") into the rdc core.
//!
//! Every operation re-uses rdc's own functions; this crate adds no new
//! credential or sync logic. See `docs/superpowers/specs/2026-06-29-native-macos-app-design.md`.

/// rdc's package version, surfaced to the app's About box.
pub fn version() -> Option<&'static str> {
    rdc::version()
}
```

- [ ] **Step 6: Update the README desktop section**

In `README.md`, replace the body under `## Desktop app (macOS)` (keep the heading and the WIP warning) so it reads:
```markdown
## Desktop app (macOS)

> [!WARNING]
> Work in progress. Not ready for use.

A native macOS app (SwiftUI) for managing connections and pulling Rossum orgs
into local folders. It bridges to the rdc core through the `rdc-ffi` crate
(in-process FFI), and shares the same on-disk project format as the CLI.
```

- [ ] **Step 7: Verify the whole workspace builds and rdc's tests still pass**

Run: `cargo build --workspace`
Expected: finishes with `Finished` and no errors (the old `rossum-local`/Tauri crate is gone; `rdc-ffi` compiles).

Run: `cargo test -p rdc --lib`
Expected: rdc's unit tests pass (unaffected by the restructure).

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "refactor(desktop): remove Tauri app, scaffold rdc-ffi workspace member

Preserve the app icon assets under assets/icons/ for the SwiftUI rewrite.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Add UniFFI scaffolding, the `FfiError` type, and the binding generator

**Files:**
- Modify: `rdc-ffi/Cargo.toml` (deps + bin)
- Modify: `rdc-ffi/src/lib.rs` (`setup_scaffolding!`, module decls, exported `ffi_version`)
- Create: `rdc-ffi/src/error.rs`
- Create: `rdc-ffi/src/bin/uniffi-bindgen.rs`

**Interfaces:**
- Consumes: `rdc_ffi::version` (Task 1).
- Produces:
  - `error::FfiError` (`#[derive(uniffi::Error)]`, variant `Operation { message: String }`).
  - `error::op(String) -> FfiError`, `error::map_err(anyhow::Error) -> FfiError`.
  - Exported `ffi_version() -> Option<String>`.
  - A `uniffi-bindgen` binary usable for Swift codegen.

> **Version note (resolve at execution):** pin `uniffi` to whatever `cargo add uniffi` resolves (expected `0.28.x`/`0.29.x`). The proc-macro surface used here — `setup_scaffolding!`, `#[uniffi::export]`, `#[derive(uniffi::Record)]`, `#[derive(uniffi::Enum)]`, `#[derive(uniffi::Error)]`, `#[uniffi::export(callback_interface)]` — is stable across 0.27–0.29. If a name differs in the resolved version, adjust to that version's documented equivalent.

- [ ] **Step 1: Add the uniffi dependency and the bindgen bin to `rdc-ffi/Cargo.toml`**

Add to `[dependencies]`:
```toml
uniffi = { version = "0.28" }
anyhow = "1"
thiserror = "2"
```
Add after the `[lib]` block:
```toml
[[bin]]
name = "uniffi-bindgen"
path = "src/bin/uniffi-bindgen.rs"
required-features = ["uniffi/cli"]
```

- [ ] **Step 2: Create the binding generator entry point**

Create `rdc-ffi/src/bin/uniffi-bindgen.rs`:
```rust
fn main() {
    uniffi::uniffi_bindgen_main()
}
```

- [ ] **Step 3: Create the error type**

Create `rdc-ffi/src/error.rs`:
```rust
//! The single error type crossing the FFI boundary. rdc returns rich
//! `anyhow::Error` chains; we flatten them to a message string the app
//! shows verbatim.

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("{message}")]
    Operation { message: String },
}

/// Build an `Operation` error from a plain message.
pub fn op(message: String) -> FfiError {
    FfiError::Operation { message }
}

/// Flatten an `anyhow::Error` (with its full `{:#}` chain) into an `FfiError`.
pub fn map_err(e: anyhow::Error) -> FfiError {
    FfiError::Operation { message: format!("{e:#}") }
}
```

- [ ] **Step 4: Wire scaffolding + an exported smoke function into lib.rs**

Replace `rdc-ffi/src/lib.rs` with:
```rust
//! FFI bridge from the native macOS app ("Rossum Local") into the rdc core.
//!
//! Every operation re-uses rdc's own functions; this crate adds no new
//! credential or sync logic. See `docs/superpowers/specs/2026-06-29-native-macos-app-design.md`.

mod error;

uniffi::setup_scaffolding!();

/// rdc's package version, surfaced to the app's About box.
pub fn version() -> Option<&'static str> {
    rdc::version()
}

/// FFI-visible version string (UniFFI cannot return `&'static str`).
#[uniffi::export]
pub fn ffi_version() -> Option<String> {
    rdc::version().map(|s| s.to_string())
}
```

- [ ] **Step 5: Write a failing test for `ffi_version`**

Append to `rdc-ffi/src/lib.rs`:
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn ffi_version_matches_rdc() {
        assert_eq!(super::ffi_version().as_deref(), rdc::version());
        assert!(super::ffi_version().is_some());
    }
}
```

- [ ] **Step 6: Run the test (and confirm scaffolding compiles)**

Run: `cargo test -p rdc-ffi ffi_version_matches_rdc`
Expected: PASS (this also proves `setup_scaffolding!`, `#[uniffi::export]`, and `FfiError` compile).

- [ ] **Step 7: Confirm the binding generator builds**

Run: `cargo build -p rdc-ffi --bin uniffi-bindgen --features uniffi/cli`
Expected: `Finished` — the `uniffi-bindgen` binary compiles.

- [ ] **Step 8: Commit**

```bash
git add rdc-ffi/Cargo.toml rdc-ffi/src/lib.rs rdc-ffi/src/error.rs rdc-ffi/src/bin/uniffi-bindgen.rs Cargo.lock
git commit -m "feat(rdc-ffi): add UniFFI scaffolding, FfiError, and bindgen bin

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Port connection discovery into `rdc-ffi`

**Files:**
- Create: `rdc-ffi/src/discover.rs`
- Modify: `rdc-ffi/src/lib.rs` (add `mod discover;`)
- Modify: `rdc-ffi/Cargo.toml` (add `toml`, `serde`)

**Interfaces:**
- Consumes: `rdc::secrets::read_secrets_file`.
- Produces (all `pub(crate)`):
  - `discover::Connection { folder: PathBuf, api_base: String, org_id: u64, auth_kind: AuthKindRaw, last_sync_unix: Option<i64>, file_count: u64 }` with `name() -> &str`, `id() -> &str`.
  - `enum AuthKindRaw { Token, Password }` (internal; the FFI-facing `AuthKind` is defined in Task 4).
  - `discover::scan(&Path) -> Vec<Connection>`, `discover::find(&Path, &str) -> Option<Connection>`, `discover::inspect(&Path) -> Option<Connection>`, `discover::count_files(&Path) -> u64`.

> Discovery takes the parent path **explicitly** — the macOS app (Phase 2) owns folder selection via a security-scoped bookmark, so the old `parent_default()`/`ROSSUM_LOCAL_PARENT` logic and the `directories` dependency are intentionally dropped.

- [ ] **Step 1: Add the parsing dependencies**

Add to `rdc-ffi/Cargo.toml` `[dependencies]`:
```toml
toml = "1"
serde = { version = "1", features = ["derive"] }
```

- [ ] **Step 2: Write the discovery module with its ported tests**

Create `rdc-ffi/src/discover.rs`:
```rust
//! Connection discovery via directory scan. A Connection is any folder
//! under the caller-supplied parent that looks like an rdc project: an
//! `rdc.toml` with an `[envs.main]` section. All state is derived from
//! on-disk artifacts — there is no registry.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct Connection {
    pub folder: PathBuf,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKindRaw,
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

impl Connection {
    pub fn name(&self) -> &str {
        self.folder.file_name().and_then(|s| s.to_str()).unwrap_or("?")
    }
    /// Folder name — unique within the parent and stable across syncs.
    pub fn id(&self) -> &str {
        self.name()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthKindRaw {
    Token,
    Password,
}

#[derive(Deserialize)]
struct RdcToml {
    envs: std::collections::BTreeMap<String, RdcEnvConfig>,
}

#[derive(Deserialize)]
struct RdcEnvConfig {
    api_base: String,
    org_id: u64,
}

pub(crate) fn scan(parent: &Path) -> Vec<Connection> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(parent) else {
        return out;
    };
    for entry in rd.flatten() {
        if let Some(conn) = inspect(&entry.path()) {
            out.push(conn);
        }
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    out
}

pub(crate) fn find(parent: &Path, name: &str) -> Option<Connection> {
    inspect(&parent.join(name))
}

pub(crate) fn inspect(folder: &Path) -> Option<Connection> {
    if !folder.is_dir() {
        return None;
    }
    let content = std::fs::read_to_string(folder.join("rdc.toml")).ok()?;
    let parsed: RdcToml = toml::from_str(&content).ok()?;
    let env = parsed.envs.get("main")?;
    let secrets = rdc::secrets::read_secrets_file(folder, "main").unwrap_or_default();
    let auth_kind = if secrets.username.is_some() {
        AuthKindRaw::Password
    } else {
        AuthKindRaw::Token
    };
    let last_sync_unix = std::fs::metadata(folder.join(".rdc/state/main.lock.json"))
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    let file_count = count_files(&folder.join("envs/main"));
    Some(Connection {
        folder: folder.to_path_buf(),
        api_base: env.api_base.clone(),
        org_id: env.org_id,
        auth_kind,
        last_sync_unix,
        file_count,
    })
}

pub(crate) fn count_files(p: &Path) -> u64 {
    fn walk(p: &Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for entry in rd.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    walk(&entry.path(), acc);
                } else if meta.is_file() {
                    *acc += 1;
                }
            }
        }
    }
    let mut n = 0;
    walk(p, &mut n);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_connection(parent: &Path, name: &str, api_base: &str, org_id: u64) {
        let folder = parent.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("rdc.toml"),
            format!("[envs.main]\napi_base = \"{api_base}\"\norg_id = {org_id}\n"),
        )
        .unwrap();
    }

    #[test]
    fn scan_finds_rdc_projects_sorts_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        seed_connection(tmp.path(), "zebra", "https://example.test/api/v1", 1);
        seed_connection(tmp.path(), "alpha", "https://other.test/api/v1", 2);
        std::fs::create_dir_all(tmp.path().join("not-a-project")).unwrap();

        let cs = scan(tmp.path());
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].name(), "alpha");
        assert_eq!(cs[1].name(), "zebra");
        assert_eq!(cs[0].org_id, 2);
    }

    #[test]
    fn find_returns_named_connection() {
        let tmp = tempfile::tempdir().unwrap();
        seed_connection(tmp.path(), "acme", "https://example.test/api/v1", 7);
        let c = find(tmp.path(), "acme").unwrap();
        assert_eq!(c.api_base, "https://example.test/api/v1");
        assert_eq!(c.org_id, 7);
        assert_eq!(c.auth_kind, AuthKindRaw::Token);
    }

    #[test]
    fn find_returns_none_for_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find(tmp.path(), "nope").is_none());
    }

    #[test]
    fn auth_kind_is_password_when_username_in_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        seed_connection(tmp.path(), "p", "https://example.test/api/v1", 1);
        rdc::secrets::save_password_credentials(&tmp.path().join("p"), "main", "u", "pw").unwrap();
        let c = find(tmp.path(), "p").unwrap();
        assert_eq!(c.auth_kind, AuthKindRaw::Password);
    }
}
```

- [ ] **Step 3: Declare the module**

Add `mod discover;` to `rdc-ffi/src/lib.rs` (below `mod error;`).

- [ ] **Step 4: Run one ported test to confirm the port is intact**

This is a near-verbatim port of already-tested logic (the original `desktop/src/discover.rs` was deleted in Task 1), so this is a regression check, not a red-green TDD cycle:

Run: `cargo test -p rdc-ffi discover::tests::scan_finds_rdc_projects_sorts_by_name`
Expected: PASS (logic ported intact). If it fails, the port introduced a regression — fix before continuing.

- [ ] **Step 5: Run the full discovery test set**

Run: `cargo test -p rdc-ffi discover::`
Expected: 4 tests pass.

- [ ] **Step 6: Commit**

```bash
git add rdc-ffi/Cargo.toml rdc-ffi/src/discover.rs rdc-ffi/src/lib.rs Cargo.lock
git commit -m "feat(rdc-ffi): port connection discovery (parent supplied explicitly)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `ConnectionSummary` DTO + exported `list_connections`

**Files:**
- Create: `rdc-ffi/src/connections.rs`
- Modify: `rdc-ffi/src/lib.rs` (add `mod connections;`)

**Interfaces:**
- Consumes: `discover::{scan, Connection, AuthKindRaw}`.
- Produces:
  - `#[derive(uniffi::Enum)] pub enum AuthKind { Token, Password }`
  - `#[derive(uniffi::Record)] pub struct ConnectionSummary { id, name, api_base: String, org_id: u64, folder: String, auth_kind: AuthKind, last_sync_unix: Option<i64>, file_count: u64 }`
  - `impl From<&discover::Connection> for ConnectionSummary`
  - `#[uniffi::export] pub fn list_connections(parent: String) -> Vec<ConnectionSummary>`

- [ ] **Step 1: Create the module with DTO, conversion, and `list_connections`**

Create `rdc-ffi/src/connections.rs`:
```rust
//! FFI-facing connection management: list/add/edit/validate. Each function
//! re-uses rdc's own file + credential helpers; nothing here reimplements
//! the on-disk format.

use crate::discover::{self, AuthKindRaw, Connection};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AuthKind {
    Token,
    Password,
}

impl From<AuthKindRaw> for AuthKind {
    fn from(r: AuthKindRaw) -> Self {
        match r {
            AuthKindRaw::Token => AuthKind::Token,
            AuthKindRaw::Password => AuthKind::Password,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ConnectionSummary {
    pub id: String,
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub folder: String,
    pub auth_kind: AuthKind,
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

impl From<&Connection> for ConnectionSummary {
    fn from(c: &Connection) -> Self {
        Self {
            id: c.id().to_string(),
            name: c.name().to_string(),
            api_base: c.api_base.clone(),
            org_id: c.org_id,
            folder: c.folder.display().to_string(),
            auth_kind: c.auth_kind.into(),
            last_sync_unix: c.last_sync_unix,
            file_count: c.file_count,
        }
    }
}

/// List every Connection under `parent`. Non-project folders are skipped.
#[uniffi::export]
pub fn list_connections(parent: String) -> Vec<ConnectionSummary> {
    discover::scan(Path::new(&parent))
        .iter()
        .map(ConnectionSummary::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(parent: &Path, name: &str) {
        let folder = parent.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("rdc.toml"),
            "[envs.main]\napi_base = \"https://example.test/api/v1\"\norg_id = 5\n",
        )
        .unwrap();
    }

    #[test]
    fn list_connections_maps_summaries() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path(), "alpha");
        let out = list_connections(tmp.path().display().to_string());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "alpha");
        assert_eq!(out[0].id, "alpha");
        assert_eq!(out[0].org_id, 5);
        assert_eq!(out[0].auth_kind, AuthKind::Token);
        assert_eq!(out[0].last_sync_unix, None);
    }
}
```

- [ ] **Step 2: Declare the module**

Add `mod connections;` to `rdc-ffi/src/lib.rs`.

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p rdc-ffi connections::tests::list_connections_maps_summaries`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add rdc-ffi/src/connections.rs rdc-ffi/src/lib.rs
git commit -m "feat(rdc-ffi): export list_connections + ConnectionSummary

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `AddConnectionInput` + exported `add_connection`

**Files:**
- Modify: `rdc-ffi/src/connections.rs`

**Interfaces:**
- Consumes: `rdc::slug::slugify_unique`, `rdc::secrets::{write_secrets_file, save_password_credentials}`, `discover::{scan, find}`, `error::{op, map_err}`.
- Produces:
  - `#[derive(uniffi::Record)] pub struct AddConnectionInput { name, api_base: String, org_id: u64, auth_kind: AuthKind, token: Option<String>, username: Option<String>, password: Option<String> }`
  - `fn write_credentials(folder: &Path, auth: AuthKind, token: Option<&str>, username: Option<&str>, password: Option<&str>) -> Result<(), FfiError>` (private, reused by Task 6)
  - `#[uniffi::export] pub fn add_connection(parent: String, input: AddConnectionInput) -> Result<ConnectionSummary, FfiError>`

- [ ] **Step 1: Add imports and the input record at the top of `connections.rs`**

Add to the `use` block:
```rust
use crate::error::{map_err, op, FfiError};
use std::collections::HashSet;
```
Add the record (below `ConnectionSummary`):
```rust
#[derive(Debug, Clone, uniffi::Record)]
pub struct AddConnectionInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}
```

- [ ] **Step 2: Add the shared credential writer and `add_connection`**

Append to `connections.rs` (above the `tests` module):
```rust
/// Write credentials via rdc's own helpers. Empty strings are rejected
/// the same way the original Tauri command did.
fn write_credentials(
    folder: &Path,
    auth: AuthKind,
    token: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<(), FfiError> {
    match auth {
        AuthKind::Token => {
            let t = token
                .filter(|s| !s.is_empty())
                .ok_or_else(|| op("Token is required.".into()))?;
            rdc::secrets::write_secrets_file(folder, "main", t, None).map_err(map_err)?;
        }
        AuthKind::Password => {
            let u = username
                .filter(|s| !s.is_empty())
                .ok_or_else(|| op("Username is required.".into()))?;
            let p = password
                .filter(|s| !s.is_empty())
                .ok_or_else(|| op("Password is required.".into()))?;
            rdc::secrets::save_password_credentials(folder, "main", u, p).map_err(map_err)?;
        }
    }
    Ok(())
}

/// Create a new Connection: write `rdc.toml` + secrets under a unique slug.
#[uniffi::export]
pub fn add_connection(
    parent: String,
    input: AddConnectionInput,
) -> Result<ConnectionSummary, FfiError> {
    let parent = std::path::PathBuf::from(parent);
    let used: HashSet<String> = discover::scan(&parent)
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let slug = rdc::slug::slugify_unique(&input.name, &used);
    let folder = parent.join(&slug);
    std::fs::create_dir_all(&folder).map_err(|e| op(format!("creating folder: {e}")))?;

    let api_base = input.api_base.trim_end_matches('/').to_string();
    let rdc_toml = format!(
        "[envs.main]\napi_base = \"{api_base}\"\norg_id = {}\n",
        input.org_id
    );
    std::fs::write(folder.join("rdc.toml"), rdc_toml)
        .map_err(|e| op(format!("writing rdc.toml: {e}")))?;

    write_credentials(
        &folder,
        input.auth_kind,
        input.token.as_deref(),
        input.username.as_deref(),
        input.password.as_deref(),
    )?;

    discover::find(&parent, &slug)
        .as_ref()
        .map(ConnectionSummary::from)
        .ok_or_else(|| op("Connection not found after add".into()))
}
```

- [ ] **Step 3: Write a failing test**

Add to the `tests` module in `connections.rs`:
```rust
#[test]
fn add_connection_writes_files_and_summary() {
    let tmp = tempfile::tempdir().unwrap();
    let input = AddConnectionInput {
        name: "Acme Prod".into(),
        api_base: "https://example.test/api/v1/".into(),
        org_id: 42,
        auth_kind: AuthKind::Token,
        token: Some("tok-123".into()),
        username: None,
        password: None,
    };
    let summary = add_connection(tmp.path().display().to_string(), input).unwrap();
    assert_eq!(summary.org_id, 42);
    assert_eq!(summary.api_base, "https://example.test/api/v1"); // trailing slash trimmed
    let folder = tmp.path().join(&summary.id);
    assert!(folder.join("rdc.toml").exists());
    assert!(folder.join("secrets/main.secrets.json").exists());
}

#[test]
fn add_connection_rejects_empty_token() {
    let tmp = tempfile::tempdir().unwrap();
    let input = AddConnectionInput {
        name: "x".into(),
        api_base: "https://example.test/api/v1".into(),
        org_id: 1,
        auth_kind: AuthKind::Token,
        token: Some(String::new()),
        username: None,
        password: None,
    };
    let err = add_connection(tmp.path().display().to_string(), input).unwrap_err();
    assert!(format!("{err}").contains("Token is required"));
}
```

- [ ] **Step 4: Run to verify it fails, then passes**

Run: `cargo test -p rdc-ffi connections::tests::add_connection_writes_files_and_summary`
Expected: PASS (implementation written in Steps 1–2).

Run: `cargo test -p rdc-ffi connections::tests::add_connection_rejects_empty_token`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rdc-ffi/src/connections.rs
git commit -m "feat(rdc-ffi): export add_connection (rdc.toml + secrets via rdc helpers)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `EditCredentialsInput` + exported `edit_credentials`

**Files:**
- Modify: `rdc-ffi/src/connections.rs`

**Interfaces:**
- Consumes: `write_credentials` (Task 5), `error::op`.
- Produces:
  - `#[derive(uniffi::Record)] pub struct EditCredentialsInput { auth_kind: AuthKind, token: Option<String>, username: Option<String>, password: Option<String> }`
  - `#[uniffi::export] pub fn edit_credentials(folder: String, input: EditCredentialsInput) -> Result<(), FfiError>`

> Wipes `secrets/main.secrets.json` before writing — this is how the original handled a token↔password mode flip without leaving stale fields. The folder is the connection's own directory (the app passes `ConnectionSummary.folder`).

- [ ] **Step 1: Add the input record**

Add to `connections.rs` (below `AddConnectionInput`):
```rust
#[derive(Debug, Clone, uniffi::Record)]
pub struct EditCredentialsInput {
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}
```

- [ ] **Step 2: Add `edit_credentials`**

Append to `connections.rs` (above `tests`):
```rust
/// Replace a Connection's stored credentials. Existing secrets are wiped
/// first so a token↔password mode flip leaves no stale fields behind.
#[uniffi::export]
pub fn edit_credentials(folder: String, input: EditCredentialsInput) -> Result<(), FfiError> {
    let folder = std::path::PathBuf::from(folder);
    if !folder.join("rdc.toml").exists() {
        return Err(op("Connection not found".into()));
    }
    let _ = std::fs::remove_file(folder.join("secrets/main.secrets.json"));
    write_credentials(
        &folder,
        input.auth_kind,
        input.token.as_deref(),
        input.username.as_deref(),
        input.password.as_deref(),
    )
}
```

- [ ] **Step 3: Write a failing test (token → password flip)**

Add to `tests`:
```rust
#[test]
fn edit_credentials_flips_token_to_password() {
    let tmp = tempfile::tempdir().unwrap();
    let added = add_connection(
        tmp.path().display().to_string(),
        AddConnectionInput {
            name: "acme".into(),
            api_base: "https://example.test/api/v1".into(),
            org_id: 1,
            auth_kind: AuthKind::Token,
            token: Some("tok".into()),
            username: None,
            password: None,
        },
    )
    .unwrap();

    edit_credentials(
        added.folder.clone(),
        EditCredentialsInput {
            auth_kind: AuthKind::Password,
            token: None,
            username: Some("user".into()),
            password: Some("pass".into()),
        },
    )
    .unwrap();

    // Discovery now reports password auth.
    let listed = list_connections(tmp.path().display().to_string());
    assert_eq!(listed[0].auth_kind, AuthKind::Password);
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p rdc-ffi connections::tests::edit_credentials_flips_token_to_password`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rdc-ffi/src/connections.rs
git commit -m "feat(rdc-ffi): export edit_credentials (wipe + rewrite secrets)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Exported `validate_existing_project`

**Files:**
- Modify: `rdc-ffi/src/connections.rs`

**Interfaces:**
- Consumes: `discover::inspect`, `error::op`.
- Produces: `#[uniffi::export] pub fn validate_existing_project(path: String) -> Result<ConnectionSummary, FfiError>`

> This only validates and returns a summary. The macOS app (Phase 2) persists a security-scoped bookmark to the external folder and tracks it — there is no symlink (the sandbox makes symlinks useless for granting access).

- [ ] **Step 1: Add `validate_existing_project`**

Append to `connections.rs` (above `tests`):
```rust
/// Validate that `path` is a single-env (`main`) rdc project and return its
/// summary. Does not move, copy, or symlink anything.
#[uniffi::export]
pub fn validate_existing_project(path: String) -> Result<ConnectionSummary, FfiError> {
    let source = std::path::PathBuf::from(&path);
    if !source.is_dir() {
        return Err(op(format!("Not a folder: {path}")));
    }
    let rdc_toml = source.join("rdc.toml");
    if !rdc_toml.exists() {
        return Err(op(format!(
            "{path} doesn't look like an rdc project (no rdc.toml). Run `rdc init` there first."
        )));
    }
    let body = std::fs::read_to_string(&rdc_toml).map_err(|e| op(format!("reading rdc.toml: {e}")))?;
    if !body.contains("[envs.main]") {
        return Err(op(format!(
            "{} has no [envs.main] section; only single-env projects named `main` are supported.",
            rdc_toml.display()
        )));
    }
    discover::inspect(&source)
        .as_ref()
        .map(ConnectionSummary::from)
        .ok_or_else(|| op("Project not discoverable".into()))
}
```

- [ ] **Step 2: Write failing tests (valid + two rejection paths)**

Add to `tests`:
```rust
#[test]
fn validate_existing_project_accepts_main_project() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path(), "proj");
    let summary =
        validate_existing_project(tmp.path().join("proj").display().to_string()).unwrap();
    assert_eq!(summary.name, "proj");
    assert_eq!(summary.org_id, 5);
}

#[test]
fn validate_existing_project_rejects_missing_toml() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("empty")).unwrap();
    let err = validate_existing_project(tmp.path().join("empty").display().to_string()).unwrap_err();
    assert!(format!("{err}").contains("no rdc.toml"));
}

#[test]
fn validate_existing_project_rejects_non_main_env() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("proj");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("rdc.toml"),
        "[envs.test]\napi_base = \"https://example.test/api/v1\"\norg_id = 1\n",
    )
    .unwrap();
    let err = validate_existing_project(folder.display().to_string()).unwrap_err();
    assert!(format!("{err}").contains("[envs.main]"));
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p rdc-ffi connections::tests::validate_existing_project`
Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add rdc-ffi/src/connections.rs
git commit -m "feat(rdc-ffi): export validate_existing_project (no symlink; sandbox-ready)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Runtime wrapper + `sync_connection` with progress callback

**Files:**
- Create: `rdc-ffi/src/sync.rs`
- Modify: `rdc-ffi/src/lib.rs` (add `mod sync;`)
- Modify: `rdc-ffi/Cargo.toml` (add `tokio`)

**Interfaces:**
- Consumes: `rdc::cli::init::write_scaffold_files`, `rdc::secrets::resolve_token`, `rdc::cli::sync::embed::sync_no_push`, `discover::count_files`, `error::FfiError`.
- Produces:
  - `pub fn block_on<F: Future>(fut: F) -> F::Output` (current-thread runtime).
  - `#[derive(uniffi::Enum)] pub enum SyncPhase { Started, Done { file_count: u64 }, Error { message: String } }`
  - `#[uniffi::export(callback_interface)] pub trait SyncProgress: Send + Sync { fn on_phase(&self, phase: SyncPhase); }`
  - `#[derive(uniffi::Record)] pub struct SyncResult { file_count: u64 }`
  - `#[uniffi::export] pub fn sync_connection(folder: String, api_base: String, org_id: u64, progress: Box<dyn SyncProgress>) -> Result<SyncResult, FfiError>`

> The sync runs `async`/`!Send` code on a fresh current-thread Tokio runtime via `block_on`, which executes on the calling thread — exactly the pattern the old Tauri backend used. The Swift side calls this from a background task and wraps it in `start/stopAccessingSecurityScopedResource` (Phase 2). The full network sync is **not** unit-tested offline; the runtime wrapper and error/phase plumbing are, and an `#[ignore]`d live test mirrors the repo's existing opt-in live-API harness convention.

- [ ] **Step 1: Add the tokio dependency**

Add to `rdc-ffi/Cargo.toml` `[dependencies]`:
```toml
tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync"] }
```
(These match the features the former desktop crate used with `Builder::new_current_thread().enable_all()`; the IO/time drivers are pulled into the unified feature set by rdc's `reqwest` dependency.)

- [ ] **Step 2: Create `sync.rs` with the runtime wrapper and a unit test**

Create `rdc-ffi/src/sync.rs`:
```rust
//! The one operation that needs rdc's async sync engine. Runs `sync_no_push`
//! on a fresh current-thread Tokio runtime (the engine holds `!Send` types
//! across awaits, so it cannot run on a multi-thread worker).

use crate::discover::count_files;
use crate::error::FfiError;
use std::future::Future;
use std::path::PathBuf;

/// Block the calling thread on `fut` using a current-thread runtime.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread tokio runtime")
        .block_on(fut)
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum SyncPhase {
    Started,
    Done { file_count: u64 },
    Error { message: String },
}

#[uniffi::export(callback_interface)]
pub trait SyncProgress: Send + Sync {
    fn on_phase(&self, phase: SyncPhase);
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SyncResult {
    pub file_count: u64,
}

/// Pull-only sync of one Connection. Scaffolds init files, resolves the
/// token (silent re-login in password mode), then runs `sync_no_push`.
#[uniffi::export]
pub fn sync_connection(
    folder: String,
    api_base: String,
    org_id: u64,
    progress: Box<dyn SyncProgress>,
) -> Result<SyncResult, FfiError> {
    let folder = PathBuf::from(folder);
    progress.on_phase(SyncPhase::Started);

    let result: anyhow::Result<u64> = block_on(async {
        rdc::cli::init::write_scaffold_files(&folder, "main", &api_base, org_id)?;
        let token = rdc::secrets::resolve_token(&folder, "main", &api_base).await?;
        rdc::cli::sync::embed::sync_no_push(&folder, "main", &token).await?;
        Ok(count_files(&folder.join("envs/main")))
    });

    match result {
        Ok(file_count) => {
            progress.on_phase(SyncPhase::Done { file_count });
            Ok(SyncResult { file_count })
        }
        Err(e) => {
            let message = format!("{e:#}");
            progress.on_phase(SyncPhase::Error { message: message.clone() });
            Err(FfiError::Operation { message })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_runs_async_to_completion() {
        let n = block_on(async { 20 + 22 });
        assert_eq!(n, 42);
    }
}
```

- [ ] **Step 3: Declare the module**

Add `mod sync;` to `rdc-ffi/src/lib.rs`.

- [ ] **Step 4: Run the runtime unit test and the full crate test suite**

Run: `cargo test -p rdc-ffi sync::tests::block_on_runs_async_to_completion`
Expected: PASS.

Run: `cargo test -p rdc-ffi`
Expected: all tests pass (discovery + connections + sync runtime + ffi_version). This confirms the entire UniFFI surface compiles together.

- [ ] **Step 5: Add an `#[ignore]`d live sync test (manual, opt-in)**

Append to the `tests` module in `sync.rs`:
```rust
struct NoopProgress;
impl SyncProgress for NoopProgress {
    fn on_phase(&self, _phase: SyncPhase) {}
}

// Live test: requires a reachable Rossum env + valid token in the project.
// Run manually with:  cargo test -p rdc-ffi -- --ignored live_sync
// Set RDC_FFI_LIVE_DIR to a folder containing rdc.toml + secrets/main.secrets.json.
#[test]
#[ignore = "hits the live Rossum API; set RDC_FFI_LIVE_DIR and run with --ignored"]
fn live_sync_pulls_files() {
    let dir = std::env::var("RDC_FFI_LIVE_DIR").expect("set RDC_FFI_LIVE_DIR");
    let toml = std::fs::read_to_string(std::path::Path::new(&dir).join("rdc.toml")).unwrap();
    let parsed: toml::Value = toml::from_str(&toml).unwrap();
    let env = &parsed["envs"]["main"];
    let api_base = env["api_base"].as_str().unwrap().to_string();
    let org_id = env["org_id"].as_integer().unwrap() as u64;

    let result = sync_connection(dir, api_base, org_id, Box::new(NoopProgress)).unwrap();
    assert!(result.file_count > 0, "expected pulled files");
}
```

- [ ] **Step 6: Verify the live test compiles but is skipped by default**

Run: `cargo test -p rdc-ffi`
Expected: the live test shows as `ignored` in the summary (e.g. `... 1 ignored`); all other tests pass.

- [ ] **Step 7: Commit**

```bash
git add rdc-ffi/Cargo.toml rdc-ffi/src/sync.rs rdc-ffi/src/lib.rs Cargo.lock
git commit -m "feat(rdc-ffi): export sync_connection with progress callback + runtime wrapper

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Build script — universal static lib + Swift bindings + xcframework

**Files:**
- Create: `rdc-ffi/build-xcframework.sh`
- Create: `rdc-ffi/.gitignore`

**Interfaces:**
- Consumes: the built `rdc-ffi` crate + the `uniffi-bindgen` bin.
- Produces: `rdc-ffi/generated/*.swift` (+ headers/modulemap) and `rdc-ffi/rdc_ffi.xcframework`, consumed by the Phase 2 Xcode project.

> **Verification caveat:** the `cargo`/`uniffi-bindgen`/`lipo` steps run anywhere with the Rust toolchain. The final `xcodebuild -create-xcframework` needs Xcode Command Line Tools — runnable on the maintainer's macOS machine. If `xcodebuild` is unavailable in the execution environment, complete through binding generation + `lipo` and leave the xcframework step for the maintainer; report exactly which step was reached.

- [ ] **Step 1: Create `.gitignore` for generated artifacts**

Create `rdc-ffi/.gitignore`:
```
/generated/
/*.xcframework/
```

- [ ] **Step 2: Create the build script**

Create `rdc-ffi/build-xcframework.sh`:
```bash
#!/usr/bin/env bash
# Build a universal (arm64 + x86_64) static lib for rdc-ffi, generate the
# Swift bindings, and assemble an .xcframework for the SwiftUI app to link.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
TARGET_DIR="$ROOT/target"
OUT="$HERE/generated"
LIB="librdc_ffi.a"
MODULE="rdc_ffi"

echo "==> Adding Apple targets"
rustup target add aarch64-apple-darwin x86_64-apple-darwin

echo "==> Building release static libs"
cargo build --release -p rdc-ffi --target aarch64-apple-darwin
cargo build --release -p rdc-ffi --target x86_64-apple-darwin

echo "==> Generating Swift bindings (library mode)"
rm -rf "$OUT"; mkdir -p "$OUT"
cargo run --release --features uniffi/cli --bin uniffi-bindgen -- \
  generate \
  --library "$TARGET_DIR/aarch64-apple-darwin/release/$LIB" \
  --language swift \
  --out-dir "$OUT"
echo "Generated files:"; ls -1 "$OUT"

echo "==> Creating universal static lib"
mkdir -p "$TARGET_DIR/universal-apple-darwin/release"
lipo -create \
  "$TARGET_DIR/aarch64-apple-darwin/release/$LIB" \
  "$TARGET_DIR/x86_64-apple-darwin/release/$LIB" \
  -output "$TARGET_DIR/universal-apple-darwin/release/$LIB"

echo "==> Assembling headers + modulemap"
HDRS="$OUT/include"; mkdir -p "$HDRS"
# UniFFI emits <Module>FFI.h and a modulemap; names can vary by version.
# Move whatever .h / .modulemap were produced into the include dir and
# normalize the modulemap filename to module.modulemap.
find "$OUT" -maxdepth 1 -name "*.h" -exec mv {} "$HDRS/" \;
find "$OUT" -maxdepth 1 -name "*.modulemap" -exec mv {} "$HDRS/module.modulemap" \;

echo "==> Creating xcframework"
rm -rf "$HERE/$MODULE.xcframework"
xcodebuild -create-xcframework \
  -library "$TARGET_DIR/universal-apple-darwin/release/$LIB" \
  -headers "$HDRS" \
  -output "$HERE/$MODULE.xcframework"

echo "==> Done"
echo "    Swift sources: $OUT/*.swift"
echo "    XCFramework:   $HERE/$MODULE.xcframework"
```

- [ ] **Step 3: Make it executable**

Run: `chmod +x rdc-ffi/build-xcframework.sh`

- [ ] **Step 4: Run the binding-generation portion and confirm Swift is emitted**

Run: `bash rdc-ffi/build-xcframework.sh`
Expected: prints "Generated files:" followed by a `.swift` file (e.g. `rdc_ffi.swift`), a `*FFI.h`, and a `*.modulemap`; then a universal `.a`; then `rdc_ffi.xcframework`. If `xcodebuild` is unavailable, expect success through the `lipo` step and a clear failure at `xcodebuild` — that is acceptable per the caveat; record where it stopped.

- [ ] **Step 5: Sanity-check the generated Swift surface**

Run: `grep -E "func (listConnections|addConnection|editCredentials|validateExistingProject|syncConnection|ffiVersion)" rdc-ffi/generated/*.swift`
Expected: all six exported functions appear (UniFFI lowerCamelCases them). If any are missing, an `#[uniffi::export]` was dropped — fix before committing.

- [ ] **Step 6: Commit (script + gitignore only; generated artifacts are ignored)**

```bash
git add rdc-ffi/build-xcframework.sh rdc-ffi/.gitignore
git commit -m "build(rdc-ffi): script to build universal lib + Swift bindings + xcframework

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Document the FFI crate and finalize Phase 1

**Files:**
- Create: `rdc-ffi/README.md`

**Interfaces:**
- Consumes: everything above.
- Produces: build/usage docs for Phase 2.

- [ ] **Step 1: Write the crate README**

Create `rdc-ffi/README.md`:
```markdown
# rdc-ffi

In-process FFI bridge from the native macOS app ("Rossum Local") to the `rdc`
core, generated with [UniFFI](https://mozilla.github.io/uniffi-rs/).

Every operation re-uses rdc's own functions — no credential or sync logic is
reimplemented here, and the on-disk project format (`rdc.toml`,
`secrets/main.secrets.json`, `envs/main`, `.rdc/state`) is identical to the
CLI's, so the app and `rdc` interoperate on the same folders.

## Exported surface

| Function | Purpose |
|---|---|
| `list_connections(parent)` | Scan a folder for single-env (`main`) rdc projects |
| `add_connection(parent, input)` | Write `rdc.toml` + secrets under a unique slug |
| `edit_credentials(folder, input)` | Wipe + rewrite a connection's secrets |
| `validate_existing_project(path)` | Validate a folder is a `main` rdc project |
| `sync_connection(folder, api_base, org_id, progress)` | Pull-only sync with a phase callback |
| `ffi_version()` | rdc package version (About box) |

File operations the sandboxed app does natively (Trash, reveal in Finder,
security-scoped bookmarks) are intentionally **not** in this crate.

## Building the artifact

\`\`\`sh
./build-xcframework.sh
\`\`\`

Produces `generated/*.swift` (the Swift bindings) and `rdc_ffi.xcframework`
(a universal arm64 + x86_64 static lib). The Xcode project links both. The
final `xcodebuild -create-xcframework` step needs Xcode Command Line Tools.

## Tests

\`\`\`sh
cargo test -p rdc-ffi             # offline unit tests
RDC_FFI_LIVE_DIR=/path/to/project \
  cargo test -p rdc-ffi -- --ignored live_sync   # live API (opt-in)
\`\`\`
```

- [ ] **Step 2: Final full-workspace verification**

Run: `cargo test --workspace`
Expected: all tests pass (rdc + rdc-ffi), live test ignored.

Run: `cargo clippy --workspace --all-targets`
Expected: no `dead_code` errors and no new warnings introduced by `rdc-ffi`. (Note: per `reference_cargo_fmt_skew`, do NOT run repo-wide `cargo fmt`; pre-existing fmt skew is not a regression.)

- [ ] **Step 3: Commit**

```bash
git add rdc-ffi/README.md
git commit -m "docs(rdc-ffi): document the FFI surface and build steps

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage** (against `2026-06-29-native-macos-app-design.md`):
- §3 FFI surface — all five functions + `ffi_version`: Tasks 4–8. ✓
- §3 async/`!Send` runtime wrapper: Task 8. ✓
- §3 pure-Swift ops (Trash/reveal/bookmarks): explicitly out of this crate (README, Task 10) — deferred to Phase 2. ✓
- §5 backward-compat (on-disk format via rdc helpers): enforced throughout; verified by `add`/`edit` tests writing real `secrets/main.secrets.json`. ✓
- §7 repo layout (remove `desktop/`, add `rdc-ffi` member, preserve icons): Task 1. ✓
- §7 `crate-type`, edition 2024, `dead_code = deny`, `panic = abort`: Task 1–2 + Global Constraints + Task 10 clippy gate. ✓
- §9 testing (port discover tests, DTO mapping, runtime, interop/live): Tasks 3, 4, 8. ✓
- **Phase 2 (SwiftUI app)** — intentionally NOT in this plan; written separately against the generated bindings.

**Placeholder scan:** No TBD/TODO/"handle errors appropriately". The one version-flex note (uniffi pin) gives an exact expected range + the stable macro list + an instruction to adjust to the resolved version — not a placeholder. The build-script header-name handling uses `find` to tolerate UniFFI version naming differences, with an exact verification grep in Task 9 Step 5.

**Type consistency:** `AuthKindRaw` (internal, discover.rs) vs `AuthKind` (FFI, connections.rs) with an explicit `From` impl — names deliberately distinct, conversion defined in Task 4. `ConnectionSummary`, `AddConnectionInput`, `EditCredentialsInput`, `SyncPhase`, `SyncProgress`, `SyncResult` referenced consistently across tasks. `block_on`, `count_files`, `discover::{scan,find,inspect}`, `error::{op,map_err}`, `FfiError::Operation { message }` consistent throughout.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-29-native-macos-app-phase1-rdc-ffi.md`.
