# Rossum Local — macOS Desktop App Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a signed, notarized macOS desktop app that pulls Rossum org configurations into local folders for Claude consumption — pull-only, manual sync, multiple Connections, credentials in macOS Keychain.

**Architecture:** A new Cargo workspace member `rossum-local/` alongside the existing `rdc` crate. Tauri 2 binary with a Rust backend that embeds `rdc` as a library and a vanilla-TS WebView frontend. Per-Connection folder under `~/Documents/Rossum/<slug>/` containing a real rdc-style snapshot. Sync invokes a new `rdc::cli::sync::embed::sync_no_push(cwd, env, token)` entry point that bypasses both `std::env::current_dir` and the on-disk secrets file.

**Tech Stack:** Rust 2024 edition, Tauri 2.x, `security-framework` (macOS Keychain), `tokio`, `serde`, `ulid`, `directories` (XDG-style paths on macOS). Frontend: vanilla TypeScript + CSS, no framework. CI: GitHub Actions on `macos-14`. Signing: Apple Developer ID Application certificate + notarytool + stapler.

---

## File Structure

**Workspace root changes:**
- Modify `Cargo.toml` — add `[workspace]` section alongside existing `[package]`.

**New rdc-side files:**
- Create `src/cli/sync/embed.rs` — public `sync_no_push(cwd, env, token)` entry point for embedders.
- Modify `src/cli/sync/mod.rs` — declare `pub mod embed`; refactor `run_cycle` to accept optional pre-resolved token.

**New `rossum-local/` crate:**
- `rossum-local/Cargo.toml` — package manifest, Tauri 2 + rdc deps.
- `rossum-local/tauri.conf.json` — Tauri runtime config (bundle id, window size, plugins).
- `rossum-local/build.rs` — Tauri build script.
- `rossum-local/src/main.rs` — app entry point; minimal Tauri builder.
- `rossum-local/src/lib.rs` — module declarations.
- `rossum-local/src/connection.rs` — `Connection` struct + JSON serde.
- `rossum-local/src/registry.rs` — load/save `connections.json` atomically.
- `rossum-local/src/settings.rs` — load/save `settings.json` atomically.
- `rossum-local/src/keychain.rs` — macOS Keychain wrapper with test fake.
- `rossum-local/src/auth.rs` — silent re-login + token expiry handling.
- `rossum-local/src/sync.rs` — orchestrator wrapping `rdc::cli::sync::embed::sync_no_push`.
- `rossum-local/src/commands.rs` — Tauri command handlers (`#[tauri::command]`).
- `rossum-local/src/paths.rs` — Application Support, default folder parent.
- `rossum-local/src/folder.rs` — Reveal in Finder, Copy path, Trash on remove.
- `rossum-local/src/error.rs` — typed errors + Display for user-facing messages.
- `rossum-local/ui/index.html` — empty state, sidebar+detail layout.
- `rossum-local/ui/main.ts` — frontend logic.
- `rossum-local/ui/styles.css` — light/dark theme.
- `rossum-local/icons/*` — placeholder icons (generated).

**CI:**
- `.github/workflows/desktop-release.yml` — signed DMG build on tag.

**Docs:**
- Modify `README.md` — new "Desktop app (macOS)" section linking to releases.

---

## Phase 1 — Workspace + Tauri scaffolding

### Task 1: Convert root `Cargo.toml` to a workspace alongside the existing package

**Files:**
- Modify: `Cargo.toml` (root)

- [ ] **Step 1: Verify current Cargo.toml has no `[workspace]` section yet**

Run: `grep -n "^\[workspace\]" Cargo.toml`
Expected: no output (workspace not yet declared).

- [ ] **Step 2: Add a hybrid package+workspace section**

Insert immediately after the existing `[lints.rust]` block at the end of `Cargo.toml`:

```toml

[workspace]
members = [".", "rossum-local"]

[workspace.package]
edition = "2024"
license = "WTFPL"
```

The root crate (`rdc`) is the `.` member; `rossum-local` will be added in Task 2.

- [ ] **Step 3: Verify Cargo accepts the workspace before `rossum-local/` exists**

Cargo refuses to load a workspace listing a non-existent member. Temporarily remove `"rossum-local"` from `members` to verify the rest works:

```toml
[workspace]
members = ["."]
```

Run: `cargo metadata --format-version 1 --no-deps > /dev/null`
Expected: exit code 0, no errors.

- [ ] **Step 4: Restore the full `members` list**

Put `"rossum-local"` back:

```toml
[workspace]
members = [".", "rossum-local"]
```

(`cargo metadata` will now error until Task 2 creates the subdirectory — that's expected.)

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml
git commit -m "build(workspace): convert root crate into hybrid package+workspace for rossum-local sibling"
```

---

### Task 2: Scaffold the `rossum-local` Tauri 2 crate

**Files:**
- Create: `rossum-local/Cargo.toml`
- Create: `rossum-local/build.rs`
- Create: `rossum-local/tauri.conf.json`
- Create: `rossum-local/src/main.rs`
- Create: `rossum-local/src/lib.rs`
- Create: `rossum-local/ui/index.html`
- Create: `rossum-local/.gitignore`

- [ ] **Step 1: Create the directory and Cargo manifest**

```bash
mkdir -p rossum-local/src rossum-local/ui rossum-local/icons
```

Write `rossum-local/Cargo.toml`:

```toml
[package]
name = "rossum-local"
version = "0.1.0"
edition.workspace = true
license.workspace = true
description = "Pull Rossum organizations locally for Claude consumption"

[lib]
name = "rossum_local"
path = "src/lib.rs"

[[bin]]
name = "rossum-local"
path = "src/main.rs"

[build-dependencies]
tauri-build = { version = "2", features = [] }

[dependencies]
rdc = { path = ".." }
tauri = { version = "2", features = ["macos-private-api"] }
tauri-plugin-shell = "2"
tauri-plugin-clipboard-manager = "2"
tauri-plugin-dialog = "2"
tauri-plugin-updater = "2"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync", "parking_lot"] }
tokio-util = { version = "0.7", features = ["rt"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
thiserror = "2"
ulid = { version = "1", features = ["serde"] }
directories = "5"
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
url = "2"

[target.'cfg(target_os = "macos")'.dependencies]
security-framework = "2"

[dev-dependencies]
wiremock = "0.6"
tempfile = "3"
```

- [ ] **Step 2: Create `rossum-local/build.rs`**

```rust
fn main() {
    tauri_build::build();
}
```

- [ ] **Step 3: Create the Tauri runtime config**

Write `rossum-local/tauri.conf.json`:

```json
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "Rossum Local",
  "identifier": "ai.rossum.local",
  "version": "0.1.0",
  "build": {
    "frontendDist": "ui",
    "devUrl": null
  },
  "app": {
    "windows": [
      {
        "title": "Rossum Local",
        "width": 860,
        "height": 560,
        "minWidth": 600,
        "minHeight": 400,
        "resizable": true,
        "fullscreen": false
      }
    ],
    "macOSPrivateApi": true,
    "security": {
      "csp": "default-src 'self'; style-src 'self' 'unsafe-inline'"
    }
  },
  "bundle": {
    "active": true,
    "targets": ["dmg"],
    "icon": ["icons/icon.icns"],
    "category": "DeveloperTool",
    "shortDescription": "Pull Rossum orgs locally for Claude",
    "longDescription": "Rossum Local pulls a Rossum organization's configuration (workspaces, queues, schemas, hooks, rules, formulas, MDH) into a local folder so Claude (Code or the desktop app) can read it.",
    "macOS": {
      "minimumSystemVersion": "12.0",
      "hardenedRuntime": true,
      "signingIdentity": null,
      "entitlements": null
    }
  },
  "plugins": {}
}
```

`signingIdentity: null` runs locally as ad-hoc-signed; the CI workflow in Task 24 overrides at build time.

- [ ] **Step 4: Create minimal Rust entry points**

`rossum-local/src/lib.rs`:

```rust
// Module skeleton. Each module is filled in by subsequent tasks.
```

`rossum-local/src/main.rs`:

```rust
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .run(tauri::generate_context!())
        .expect("error while running Rossum Local");
}
```

- [ ] **Step 5: Create a placeholder index.html**

`rossum-local/ui/index.html`:

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <title>Rossum Local</title>
  <meta http-equiv="Content-Security-Policy" content="default-src 'self'; style-src 'self' 'unsafe-inline'" />
</head>
<body>
  <h1>Rossum Local — scaffold</h1>
  <p>Window opens. UI follows in later tasks.</p>
</body>
</html>
```

- [ ] **Step 6: Create `rossum-local/.gitignore`**

```
/target
/dist
*.icns
*.png
!icons/.gitkeep
```

Also add `rossum-local/icons/.gitkeep` (empty file) to keep the icons directory tracked.

- [ ] **Step 7: Generate placeholder icon**

Tauri requires `icons/icon.icns` at build time. Generate a placeholder via the Tauri CLI:

```bash
cargo install tauri-cli --version "^2.0.0" --locked
cd rossum-local
# Tauri ships a default icon generator; for now copy a 1024x1024 plain placeholder:
# (engineer: replace with the real icon when available — see Task 25)
cargo tauri icon icons/icon.png 2>/dev/null || true
```

If the engineer doesn't yet have an `icon.png` source, create a 1024×1024 amber-tinted square in any image editor and save as `icons/icon.png`, then re-run `cargo tauri icon icons/icon.png` from the `rossum-local/` directory.

- [ ] **Step 8: Verify the workspace builds**

Run: `cargo build --package rossum-local`
Expected: build succeeds with warnings only.

- [ ] **Step 9: Commit**

```bash
git add rossum-local/ Cargo.toml
git commit -m "feat(rossum-local): scaffold Tauri 2 crate with rdc dependency"
```

---

### Task 3: Verify `cargo tauri dev` opens an empty window

**Files:** none modified.

- [ ] **Step 1: Run the dev command**

```bash
cd rossum-local
cargo tauri dev
```

Expected: an 860×560 window titled "Rossum Local" opens with the placeholder HTML visible. Quit via Cmd-Q.

- [ ] **Step 2: Diagnose if it fails**

If the window does not open:

- "icons/icon.icns: No such file" → return to Task 2 Step 7 and run `cargo tauri icon`.
- "Failed to bundle project" → likely a malformed `tauri.conf.json`; validate JSON syntax.
- macOS Gatekeeper prompt → ad-hoc-signed dev builds may trigger a one-time approve dialog; click Open.

- [ ] **Step 3: No commit needed**

Pure verification. Move to Task 4.

---

## Phase 2 — App registry, settings, paths

### Task 4: `Connection` struct + registry serde round-trip

**Files:**
- Create: `rossum-local/src/connection.rs`
- Create: `rossum-local/src/paths.rs`
- Create: `rossum-local/tests/registry_roundtrip.rs`
- Modify: `rossum-local/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `rossum-local/tests/registry_roundtrip.rs`:

```rust
use rossum_local::connection::{AuthKind, Connection, ConnectionStatus};
use rossum_local::registry::Registry;
use ulid::Ulid;

#[test]
fn registry_roundtrips_via_json() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("connections.json");

    let conn = Connection {
        id: Ulid::new(),
        name: "Acme Corp — Production".into(),
        slug: "acme-corp-production".into(),
        api_base: "https://acme.app.rossum.ai/api/v1".into(),
        org_id: 12345,
        folder: tmp.path().join("acme-corp-production"),
        auth_kind: AuthKind::Password,
        last_sync_unix: Some(1763500000),
        last_status: ConnectionStatus::Ok,
        file_count: 287,
    };

    let mut reg = Registry::default();
    reg.upsert(conn.clone());
    reg.save(&path).unwrap();

    let loaded = Registry::load(&path).unwrap();
    assert_eq!(loaded.connections().len(), 1);
    assert_eq!(loaded.connections()[0].name, conn.name);
    assert_eq!(loaded.connections()[0].slug, conn.slug);
    assert_eq!(loaded.connections()[0].api_base, conn.api_base);
    assert_eq!(loaded.connections()[0].org_id, conn.org_id);
    assert_eq!(loaded.connections()[0].auth_kind, AuthKind::Password);
    assert_eq!(loaded.connections()[0].last_sync_unix, Some(1763500000));
    assert_eq!(loaded.connections()[0].file_count, 287);
}

#[test]
fn registry_load_missing_file_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("does-not-exist.json");
    let reg = Registry::load(&path).unwrap();
    assert!(reg.connections().is_empty());
}
```

- [ ] **Step 2: Run the test — verify it fails to compile**

Run: `cargo test --package rossum-local --test registry_roundtrip`
Expected: compile errors (`rossum_local::connection` doesn't exist yet).

- [ ] **Step 3: Implement `connection.rs`**

Write `rossum-local/src/connection.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use ulid::Ulid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    Token,
    Password,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum ConnectionStatus {
    Never,
    Ok,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: Ulid,
    pub name: String,
    pub slug: String,
    pub api_base: String,
    pub org_id: u64,
    pub folder: PathBuf,
    pub auth_kind: AuthKind,
    #[serde(default)]
    pub last_sync_unix: Option<i64>,
    #[serde(default = "default_status")]
    pub last_status: ConnectionStatus,
    #[serde(default)]
    pub file_count: u64,
}

fn default_status() -> ConnectionStatus {
    ConnectionStatus::Never
}
```

- [ ] **Step 4: Implement `registry.rs`**

Write `rossum-local/src/registry.rs`:

```rust
use crate::connection::Connection;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use ulid::Ulid;

const REGISTRY_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Registry {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    connections: Vec<Connection>,
}

fn default_version() -> u32 {
    REGISTRY_VERSION
}

impl Registry {
    pub fn connections(&self) -> &[Connection] {
        &self.connections
    }

    pub fn upsert(&mut self, conn: Connection) {
        if let Some(slot) = self.connections.iter_mut().find(|c| c.id == conn.id) {
            *slot = conn;
        } else {
            self.connections.push(conn);
        }
    }

    pub fn remove(&mut self, id: Ulid) -> Option<Connection> {
        let pos = self.connections.iter().position(|c| c.id == id)?;
        Some(self.connections.remove(pos))
    }

    pub fn get(&self, id: Ulid) -> Option<&Connection> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn used_slugs(&self) -> std::collections::HashSet<String> {
        self.connections.iter().map(|c| c.slug.clone()).collect()
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let mut reg: Registry = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if reg.version != REGISTRY_VERSION {
                    anyhow::bail!(
                        "unsupported registry version {} (expected {})",
                        reg.version,
                        REGISTRY_VERSION
                    );
                }
                // Defensive: deduplicate by id (in case the file was hand-edited)
                let mut seen = std::collections::HashSet::new();
                reg.connections.retain(|c| seen.insert(c.id));
                Ok(reg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                version: REGISTRY_VERSION,
                connections: Vec::new(),
            }),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .context("serializing registry")?;
        write_atomic(path, &bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
```

- [ ] **Step 5: Implement `paths.rs` (Application Support locations)**

Write `rossum-local/src/paths.rs`:

```rust
use anyhow::{Context, Result};
use std::path::PathBuf;

const BUNDLE_ID: &str = "ai.rossum.local";

/// `~/Library/Application Support/ai.rossum.local/`
pub fn app_support_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("ai", "rossum", "local")
        .context("resolving Application Support directory")?;
    Ok(dirs.data_dir().to_path_buf())
}

pub fn registry_path() -> Result<PathBuf> {
    Ok(app_support_dir()?.join("connections.json"))
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(app_support_dir()?.join("settings.json"))
}

/// `~/Documents/Rossum/` by default.
pub fn default_folder_parent() -> Result<PathBuf> {
    let home = directories::UserDirs::new()
        .context("resolving user directories")?;
    let docs = home
        .document_dir()
        .context("no Documents directory on this system")?;
    Ok(docs.join("Rossum"))
}

pub fn keychain_service() -> &'static str {
    BUNDLE_ID
}
```

- [ ] **Step 6: Wire modules into `lib.rs`**

Replace `rossum-local/src/lib.rs` with:

```rust
pub mod connection;
pub mod paths;
pub mod registry;
```

- [ ] **Step 7: Run the tests — verify they pass**

Run: `cargo test --package rossum-local --test registry_roundtrip`
Expected: 2 passed, 0 failed.

- [ ] **Step 8: Commit**

```bash
git add rossum-local/src/connection.rs rossum-local/src/paths.rs rossum-local/src/registry.rs rossum-local/src/lib.rs rossum-local/tests/registry_roundtrip.rs rossum-local/Cargo.toml
git commit -m "feat(rossum-local): Connection struct + Registry with atomic JSON persistence"
```

---

### Task 5: App settings (default folder parent + update channel)

**Files:**
- Create: `rossum-local/src/settings.rs`
- Create: `rossum-local/tests/settings_roundtrip.rs`
- Modify: `rossum-local/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `rossum-local/tests/settings_roundtrip.rs`:

```rust
use rossum_local::settings::{Settings, UpdateChannel};

#[test]
fn settings_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("settings.json");

    let s = Settings {
        version: 1,
        default_folder_parent: tmp.path().join("Rossum"),
        update_channel: UpdateChannel::Stable,
    };
    s.save(&path).unwrap();

    let loaded = Settings::load(&path).unwrap();
    assert_eq!(loaded.default_folder_parent, s.default_folder_parent);
    assert_eq!(loaded.update_channel, UpdateChannel::Stable);
}

#[test]
fn settings_load_missing_returns_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("missing.json");
    let s = Settings::load(&path).unwrap();
    assert_eq!(s.update_channel, UpdateChannel::Stable);
}
```

- [ ] **Step 2: Run — confirm compile errors**

Run: `cargo test --package rossum-local --test settings_roundtrip`
Expected: `unresolved import rossum_local::settings`.

- [ ] **Step 3: Implement `settings.rs`**

Write `rossum-local/src/settings.rs`:

```rust
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    Stable,
    Beta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub version: u32,
    pub default_folder_parent: PathBuf,
    #[serde(default = "default_channel")]
    pub update_channel: UpdateChannel,
}

fn default_channel() -> UpdateChannel {
    UpdateChannel::Stable
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::defaults()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).context("serializing settings")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn defaults() -> Self {
        let parent = crate::paths::default_folder_parent()
            .unwrap_or_else(|_| PathBuf::from("Rossum"));
        Self {
            version: 1,
            default_folder_parent: parent,
            update_channel: UpdateChannel::Stable,
        }
    }
}
```

- [ ] **Step 4: Add to `lib.rs`**

```rust
pub mod connection;
pub mod paths;
pub mod registry;
pub mod settings;
```

- [ ] **Step 5: Run tests — passing**

Run: `cargo test --package rossum-local --test settings_roundtrip`
Expected: 2 passed.

- [ ] **Step 6: Commit**

```bash
git add rossum-local/src/settings.rs rossum-local/src/lib.rs rossum-local/tests/settings_roundtrip.rs
git commit -m "feat(rossum-local): app Settings with defaults and atomic persistence"
```

---

### Task 6: Slug derivation reusing rdc's `slugify` + collision suffix

**Files:**
- Create: `rossum-local/src/slug.rs`
- Create: `rossum-local/tests/slug.rs`
- Modify: `rossum-local/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `rossum-local/tests/slug.rs`:

```rust
use rossum_local::slug::derive_slug;
use std::collections::HashSet;

#[test]
fn slug_strips_non_ascii() {
    let used = HashSet::new();
    assert_eq!(derive_slug("Faktura č. 1", &used), "faktura-1");
}

#[test]
fn slug_collision_appends_suffix() {
    let mut used = HashSet::new();
    used.insert("acme-corp-production".to_string());
    assert_eq!(
        derive_slug("Acme Corp — Production", &used),
        "acme-corp-production-2"
    );
}

#[test]
fn slug_empty_input_falls_back() {
    let used = HashSet::new();
    assert_eq!(derive_slug("!!!", &used), "_unnamed");
}
```

- [ ] **Step 2: Run — confirm fail**

Run: `cargo test --package rossum-local --test slug`
Expected: `unresolved import`.

- [ ] **Step 3: Implement (delegate to rdc)**

Write `rossum-local/src/slug.rs`:

```rust
use std::collections::HashSet;

/// Derive a folder slug from a user-visible Connection name.
///
/// Delegates to `rdc::slug::slugify_unique`, which (a) lowercases, (b)
/// drops non-ASCII characters, (c) collapses non-alphanumeric runs to a
/// single hyphen, (d) appends `-2`, `-3`, ... when the base slug
/// already exists in `used`.
pub fn derive_slug(name: &str, used: &HashSet<String>) -> String {
    rdc::slug::slugify_unique(name, used)
}
```

- [ ] **Step 4: Add to `lib.rs`**

```rust
pub mod connection;
pub mod paths;
pub mod registry;
pub mod settings;
pub mod slug;
```

- [ ] **Step 5: Run — passing**

Run: `cargo test --package rossum-local --test slug`
Expected: 3 passed.

- [ ] **Step 6: Commit**

```bash
git add rossum-local/src/slug.rs rossum-local/src/lib.rs rossum-local/tests/slug.rs
git commit -m "feat(rossum-local): slug derivation delegating to rdc::slug::slugify_unique"
```

---

## Phase 3 — Keychain integration

### Task 7: Keychain wrapper with test fake

**Files:**
- Create: `rossum-local/src/keychain/mod.rs`
- Create: `rossum-local/src/keychain/macos.rs`
- Create: `rossum-local/src/keychain/fake.rs`
- Create: `rossum-local/tests/keychain_fake.rs`
- Modify: `rossum-local/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `rossum-local/tests/keychain_fake.rs`:

```rust
use rossum_local::keychain::{fake::InMemoryKeychain, Keychain, TokenEntry};
use ulid::Ulid;

#[test]
fn fake_roundtrips_token() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();

    let entry = TokenEntry {
        token: "rsk_abc".into(),
        expires_at_unix: Some(1_800_000_000),
    };
    kc.put_token(id, &entry).unwrap();
    let got = kc.get_token(id).unwrap().unwrap();
    assert_eq!(got.token, "rsk_abc");
    assert_eq!(got.expires_at_unix, Some(1_800_000_000));
}

#[test]
fn fake_returns_none_for_missing() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    assert!(kc.get_token(id).unwrap().is_none());
}

#[test]
fn fake_password_roundtrip() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_credentials(id, "alice@acme.com", "swordfish").unwrap();
    let (u, p) = kc.get_credentials(id).unwrap().unwrap();
    assert_eq!(u, "alice@acme.com");
    assert_eq!(p, "swordfish");
}

#[test]
fn fake_delete_removes_all_entries() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry {
            token: "t".into(),
            expires_at_unix: None,
        },
    )
    .unwrap();
    kc.put_credentials(id, "u", "p").unwrap();
    kc.delete_all(id).unwrap();
    assert!(kc.get_token(id).unwrap().is_none());
    assert!(kc.get_credentials(id).unwrap().is_none());
}
```

- [ ] **Step 2: Run — confirm fail**

Run: `cargo test --package rossum-local --test keychain_fake`
Expected: `unresolved import`.

- [ ] **Step 3: Implement the trait + entry types**

Write `rossum-local/src/keychain/mod.rs`:

```rust
use anyhow::Result;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

pub mod fake;
#[cfg(target_os = "macos")]
pub mod macos;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenEntry {
    pub token: String,
    /// `None` for raw tokens (token-mode Connections) where the app has
    /// no derived expiry. `Some(unix_seconds)` for tokens issued via
    /// `/v1/auth/login` where the app assumes the documented 162 h
    /// default lifetime.
    pub expires_at_unix: Option<i64>,
}

pub trait Keychain: Send + Sync {
    fn put_token(&self, id: Ulid, entry: &TokenEntry) -> Result<()>;
    fn get_token(&self, id: Ulid) -> Result<Option<TokenEntry>>;

    fn put_credentials(&self, id: Ulid, username: &str, password: &str) -> Result<()>;
    fn get_credentials(&self, id: Ulid) -> Result<Option<(String, String)>>;

    fn delete_all(&self, id: Ulid) -> Result<()>;
}
```

- [ ] **Step 4: Implement the in-memory fake**

Write `rossum-local/src/keychain/fake.rs`:

```rust
use super::{Keychain, TokenEntry};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;
use ulid::Ulid;

#[derive(Default)]
pub struct InMemoryKeychain {
    tokens: Mutex<HashMap<Ulid, TokenEntry>>,
    creds: Mutex<HashMap<Ulid, (String, String)>>,
}

impl Keychain for InMemoryKeychain {
    fn put_token(&self, id: Ulid, entry: &TokenEntry) -> Result<()> {
        self.tokens.lock().unwrap().insert(id, entry.clone());
        Ok(())
    }

    fn get_token(&self, id: Ulid) -> Result<Option<TokenEntry>> {
        Ok(self.tokens.lock().unwrap().get(&id).cloned())
    }

    fn put_credentials(&self, id: Ulid, username: &str, password: &str) -> Result<()> {
        self.creds
            .lock()
            .unwrap()
            .insert(id, (username.to_string(), password.to_string()));
        Ok(())
    }

    fn get_credentials(&self, id: Ulid) -> Result<Option<(String, String)>> {
        Ok(self.creds.lock().unwrap().get(&id).cloned())
    }

    fn delete_all(&self, id: Ulid) -> Result<()> {
        self.tokens.lock().unwrap().remove(&id);
        self.creds.lock().unwrap().remove(&id);
        Ok(())
    }
}
```

- [ ] **Step 5: Implement the macOS Keychain backend**

Write `rossum-local/src/keychain/macos.rs`:

```rust
use super::{Keychain, TokenEntry};
use anyhow::{Context, Result};
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use ulid::Ulid;

pub struct MacOsKeychain;

fn token_service() -> &'static str {
    crate::paths::keychain_service()
}

fn username_service() -> String {
    format!("{}.username", crate::paths::keychain_service())
}

fn password_service() -> String {
    format!("{}.password", crate::paths::keychain_service())
}

fn account(id: Ulid) -> String {
    id.to_string()
}

impl Keychain for MacOsKeychain {
    fn put_token(&self, id: Ulid, entry: &TokenEntry) -> Result<()> {
        let json = serde_json::to_vec(entry).context("serializing TokenEntry")?;
        set_generic_password(token_service(), &account(id), &json)
            .context("writing token to Keychain")?;
        Ok(())
    }

    fn get_token(&self, id: Ulid) -> Result<Option<TokenEntry>> {
        match get_generic_password(token_service(), &account(id)) {
            Ok(bytes) => {
                let entry: TokenEntry = serde_json::from_slice(&bytes)
                    .context("parsing TokenEntry from Keychain")?;
                Ok(Some(entry))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e).context("reading token from Keychain"),
        }
    }

    fn put_credentials(&self, id: Ulid, username: &str, password: &str) -> Result<()> {
        set_generic_password(&username_service(), &account(id), username.as_bytes())
            .context("writing username to Keychain")?;
        set_generic_password(&password_service(), &account(id), password.as_bytes())
            .context("writing password to Keychain")?;
        Ok(())
    }

    fn get_credentials(&self, id: Ulid) -> Result<Option<(String, String)>> {
        let u = match get_generic_password(&username_service(), &account(id)) {
            Ok(bytes) => String::from_utf8(bytes).context("non-UTF-8 username in Keychain")?,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e).context("reading username from Keychain"),
        };
        let p = match get_generic_password(&password_service(), &account(id)) {
            Ok(bytes) => String::from_utf8(bytes).context("non-UTF-8 password in Keychain")?,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e).context("reading password from Keychain"),
        };
        Ok(Some((u, p)))
    }

    fn delete_all(&self, id: Ulid) -> Result<()> {
        let _ = delete_generic_password(token_service(), &account(id));
        let _ = delete_generic_password(&username_service(), &account(id));
        let _ = delete_generic_password(&password_service(), &account(id));
        Ok(())
    }
}

fn is_not_found(e: &security_framework::base::Error) -> bool {
    e.code() == -25300 // errSecItemNotFound
}
```

- [ ] **Step 6: Add to `lib.rs`**

```rust
pub mod connection;
pub mod keychain;
pub mod paths;
pub mod registry;
pub mod settings;
pub mod slug;
```

- [ ] **Step 7: Run tests — passing**

Run: `cargo test --package rossum-local --test keychain_fake`
Expected: 4 passed.

Run: `cargo build --package rossum-local`
Expected: build succeeds (including the macOS backend compilation).

- [ ] **Step 8: Commit**

```bash
git add rossum-local/src/keychain/ rossum-local/src/lib.rs rossum-local/tests/keychain_fake.rs
git commit -m "feat(rossum-local): Keychain trait + macOS Security Framework backend + in-memory fake"
```

---

### Task 8: Auth resolution — silent re-login on expiry

**Files:**
- Create: `rossum-local/src/auth.rs`
- Create: `rossum-local/tests/auth_resolve.rs`
- Modify: `rossum-local/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `rossum-local/tests/auth_resolve.rs`:

```rust
use rossum_local::auth::{resolve_token_for_sync, ResolveError, TokenSource};
use rossum_local::connection::{AuthKind, Connection, ConnectionStatus};
use rossum_local::keychain::{fake::InMemoryKeychain, Keychain, TokenEntry};
use std::path::PathBuf;
use ulid::Ulid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_conn(id: Ulid, api_base: &str, auth: AuthKind) -> Connection {
    Connection {
        id,
        name: "t".into(),
        slug: "t".into(),
        api_base: api_base.to_string(),
        org_id: 1,
        folder: PathBuf::from("/tmp/t"),
        auth_kind: auth,
        last_sync_unix: None,
        last_status: ConnectionStatus::Never,
        file_count: 0,
    }
}

#[tokio::test]
async fn token_unexpired_is_returned_as_is() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry {
            token: "valid".into(),
            expires_at_unix: Some(i64::MAX),
        },
    )
    .unwrap();
    let conn = make_conn(id, "http://unused", AuthKind::Token);
    let TokenSource { token, .. } = resolve_token_for_sync(&conn, &kc).await.unwrap();
    assert_eq!(token, "valid");
}

#[tokio::test]
async fn expired_password_token_triggers_silent_relogin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "fresh-token",
            "domain": "x"
        })))
        .mount(&server)
        .await;

    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry {
            token: "stale".into(),
            expires_at_unix: Some(0), // long expired
        },
    )
    .unwrap();
    kc.put_credentials(id, "alice@acme.com", "swordfish").unwrap();

    let conn = make_conn(id, &server.uri(), AuthKind::Password);
    let TokenSource { token, refreshed } = resolve_token_for_sync(&conn, &kc).await.unwrap();
    assert_eq!(token, "fresh-token");
    assert!(refreshed);

    // Cache updated:
    let cached = kc.get_token(id).unwrap().unwrap();
    assert_eq!(cached.token, "fresh-token");
}

#[tokio::test]
async fn expired_token_only_returns_error() {
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry {
            token: "stale".into(),
            expires_at_unix: Some(0),
        },
    )
    .unwrap();
    let conn = make_conn(id, "http://unused", AuthKind::Token);
    let err = resolve_token_for_sync(&conn, &kc).await.unwrap_err();
    assert!(matches!(err, ResolveError::SignInExpired));
}

#[tokio::test]
async fn missing_token_password_mode_logs_in() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "fresh", "domain": "x"
        })))
        .mount(&server)
        .await;

    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_credentials(id, "alice", "pw").unwrap();
    let conn = make_conn(id, &server.uri(), AuthKind::Password);
    let TokenSource { token, .. } = resolve_token_for_sync(&conn, &kc).await.unwrap();
    assert_eq!(token, "fresh");
}
```

- [ ] **Step 2: Run — confirm fail**

Run: `cargo test --package rossum-local --test auth_resolve`
Expected: `unresolved import rossum_local::auth`.

- [ ] **Step 3: Implement `auth.rs`**

Write `rossum-local/src/auth.rs`:

```rust
use crate::connection::{AuthKind, Connection};
use crate::keychain::{Keychain, TokenEntry};
use anyhow::Context;
use serde::Deserialize;
use thiserror::Error;

const TOKEN_LIFETIME_SECS: i64 = 162 * 3600;
const EXPIRY_SKEW_SECS: i64 = 60;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("Sign-in expired. Edit credentials to enter a new token.")]
    SignInExpired,
    #[error("No credentials stored for this Connection. Edit credentials to set them.")]
    Missing,
    #[error("Wrong username or password.")]
    BadPassword,
    #[error("Couldn't reach Rossum at {0}. Check your internet connection.")]
    Network(String),
    #[error("Rossum returned an unexpected error: {0}")]
    Other(String),
}

#[derive(Debug, Clone)]
pub struct TokenSource {
    pub token: String,
    pub refreshed: bool,
}

pub async fn resolve_token_for_sync<K: Keychain + ?Sized>(
    conn: &Connection,
    kc: &K,
) -> Result<TokenSource, ResolveError> {
    let now = now_unix();

    // Step 1: try cached token.
    if let Some(entry) = kc.get_token(conn.id).map_err(io_err)? {
        match entry.expires_at_unix {
            None => return Ok(TokenSource { token: entry.token, refreshed: false }),
            Some(exp) if exp > now + EXPIRY_SKEW_SECS => {
                return Ok(TokenSource { token: entry.token, refreshed: false });
            }
            _ => {} // expired; fall through
        }
    }

    // Step 2: fall back per auth kind.
    match conn.auth_kind {
        AuthKind::Token => Err(ResolveError::SignInExpired),
        AuthKind::Password => {
            let creds = kc
                .get_credentials(conn.id)
                .map_err(io_err)?
                .ok_or(ResolveError::Missing)?;
            let token = login(&conn.api_base, &creds.0, &creds.1).await?;
            let entry = TokenEntry {
                token: token.clone(),
                expires_at_unix: Some(now + TOKEN_LIFETIME_SECS),
            };
            kc.put_token(conn.id, &entry).map_err(io_err)?;
            Ok(TokenSource { token, refreshed: true })
        }
    }
}

#[derive(Deserialize)]
struct LoginResponse {
    key: String,
    #[allow(dead_code)]
    domain: Option<String>,
}

pub(crate) async fn login(api_base: &str, username: &str, password: &str) -> Result<String, ResolveError> {
    let url = format!("{}/auth/login", api_base.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .map_err(|e| ResolveError::Network(e.to_string()))?;
    if resp.status() == 401 {
        return Err(ResolveError::BadPassword);
    }
    if !resp.status().is_success() {
        return Err(ResolveError::Other(format!("{} {}", resp.status(), url)));
    }
    let body: LoginResponse = resp
        .json()
        .await
        .map_err(|e| ResolveError::Other(format!("parsing login response: {e}")))?;
    Ok(body.key)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn io_err(e: anyhow::Error) -> ResolveError {
    ResolveError::Other(format!("{e:#}"))
}
```

- [ ] **Step 4: Add to `lib.rs`**

```rust
pub mod auth;
pub mod connection;
pub mod keychain;
pub mod paths;
pub mod registry;
pub mod settings;
pub mod slug;
```

- [ ] **Step 5: Run tests — passing**

Run: `cargo test --package rossum-local --test auth_resolve`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
git add rossum-local/src/auth.rs rossum-local/src/lib.rs rossum-local/tests/auth_resolve.rs
git commit -m "feat(rossum-local): auth::resolve_token_for_sync with silent re-login + expiry skew"
```

---

## Phase 4 — rdc-side embedding hook

### Task 9: Add `rdc::cli::sync::embed::sync_no_push` and refactor `run_cycle` to accept a token

**Files:**
- Create: `src/cli/sync/embed.rs`
- Modify: `src/cli/sync/mod.rs`
- Modify: `src/cli/mod.rs` (call-site)
- Create: `tests/embed_sync.rs`

This is the only rdc-side change. It adds a public entry point for embedders that bypasses (a) `std::env::current_dir` by accepting an explicit `cwd: &Path`, and (b) the on-disk `secrets/<env>.secrets.json` by accepting a pre-resolved token. It does NOT change any existing CLI behavior.

- [ ] **Step 1: Read the current `run_cycle` signature**

Open `src/cli/sync/mod.rs` and find `pub(crate) async fn run_cycle` (around line 184). It currently calls `resolve_token(&cwd, env, &env_cfg.api_base).await?` to get the token.

- [ ] **Step 2: Write the failing test**

Create `tests/embed_sync.rs`:

```rust
//! End-to-end test of the embedding entry point against a wiremock'd
//! Rossum. Exercises a no-push pull into a tempdir.

use rdc::cli::sync::embed::sync_no_push;
use tempfile::tempdir;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn embed_sync_no_push_pulls_into_tempdir() {
    let server = MockServer::start().await;

    // Minimal Rossum surface: organization GET + empty listings.
    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "name": "test-org", "url": format!("{}/organizations/1", server.uri())
        })))
        .mount(&server)
        .await;

    for kind in [
        "workspaces", "queues", "schemas", "inboxes", "hooks", "rules",
        "labels", "engines", "engine_fields", "workflows", "workflow_steps",
        "email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{}", kind)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [], "pagination": {"next": null, "total_pages": 1, "total": 0}
            })))
            .mount(&server)
            .await;
    }
    let _ = query_param; // suppress unused warning if not needed

    let tmp = tempdir().unwrap();
    let cwd = tmp.path();

    // Seed rdc.toml that points at our wiremock.
    std::fs::write(
        cwd.join("rdc.toml"),
        format!(
            r#"[envs.main]
api_base = "{}"
org_id = 1
"#,
            server.uri()
        ),
    )
    .unwrap();

    sync_no_push(cwd, "main", "fake-token")
        .await
        .expect("sync_no_push succeeds");

    assert!(cwd.join("envs/main/_index.md").exists());
    assert!(cwd.join("envs/main/organization.json").exists());
}
```

- [ ] **Step 3: Run — confirm fail**

Run: `cargo test --test embed_sync embed_sync_no_push_pulls_into_tempdir`
Expected: `unresolved import rdc::cli::sync::embed`.

- [ ] **Step 4: Refactor `run_cycle` to accept an optional pre-resolved token**

In `src/cli/sync/mod.rs`, change the `run_cycle` signature from:

```rust
pub(crate) async fn run_cycle(
    env: &str,
    interactive: bool,
    dry_run: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    renderer: Option<Arc<Log>>,
) -> Result<CycleOutcome> {
```

to:

```rust
pub(crate) async fn run_cycle(
    env: &str,
    interactive: bool,
    dry_run: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    renderer: Option<Arc<Log>>,
    cwd_override: Option<&std::path::Path>,
    token_override: Option<String>,
) -> Result<CycleOutcome> {
```

Inside the function body, replace:

```rust
let cwd = std::env::current_dir().context("getting current directory")?;
```

with:

```rust
let cwd = match cwd_override {
    Some(p) => p.to_path_buf(),
    None => std::env::current_dir().context("getting current directory")?,
};
```

And replace:

```rust
let token = resolve_token(&cwd, env, &env_cfg.api_base).await?;
```

with:

```rust
let token = match token_override.clone() {
    Some(t) => t,
    None => resolve_token(&cwd, env, &env_cfg.api_base).await?,
};
```

- [ ] **Step 5: Update the existing `run_cycle` callers**

Two existing call sites — `pub async fn run` (line ~146) and `cli::sync::watch::run_watch`. Update both to pass `None, None` for the two new params.

In `src/cli/sync/mod.rs::run`:

```rust
run_cycle(
    env, interactive, dry_run, allow_deletes, no_push, no_pull, None,
    None, None,
)
.await?;
```

In `src/cli/sync/watch.rs`, find every call to `run_cycle` and append `, None, None` to the argument list. Run `grep -n "run_cycle(" src/cli/sync/watch.rs` to find them.

- [ ] **Step 6: Verify the existing rdc test suite still passes**

Run: `cargo test --package rdc`
Expected: same pass/fail counts as before the refactor; no new failures.

If any rdc test fails, the refactor has a bug — diagnose by reading the failing test's assertion against the new control flow.

- [ ] **Step 7: Create the `embed` module**

Write `src/cli/sync/embed.rs`:

```rust
//! Embedding entry point for non-CLI consumers (e.g. the Rossum Local
//! macOS app).
//!
//! Bypasses two CLI-only assumptions:
//! - `std::env::current_dir()` for locating `rdc.toml` and the snapshot.
//! - On-disk `secrets/<env>.secrets.json` for the API token.
//!
//! The caller supplies both explicitly. Everything else (fetch,
//! decode, atomic write, lockfile, `_index.md`) reuses the existing
//! sync pipeline in no-push, non-interactive mode.

use crate::cli::sync::CycleOutcome;
use anyhow::Result;
use std::path::Path;

/// Run one no-push reconciliation cycle.
///
/// - `cwd`: project root containing `rdc.toml`.
/// - `env`: env name (the desktop app always uses `"main"`).
/// - `token`: pre-resolved API token; the secrets file is not touched.
///
/// Returns `CycleOutcome` with per-class counts. Errors propagate as
/// `anyhow::Error`; the caller surfaces them to the user.
pub async fn sync_no_push(cwd: &Path, env: &str, token: &str) -> Result<CycleOutcome> {
    let paths = crate::paths::Paths::for_env(cwd, env);
    let _lock = crate::cli::sync::lock::EnvLock::acquire(
        &paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;
    crate::cli::sync::run_cycle(
        env,
        false, // interactive
        false, // dry_run
        false, // allow_deletes
        true,  // no_push  <-- the embedding contract
        false, // no_pull
        None,
        Some(cwd),
        Some(token.to_string()),
    )
    .await
}
```

- [ ] **Step 8: Declare the module**

In `src/cli/sync/mod.rs`, add to the existing module declarations near line 106:

```rust
pub mod classify;
pub mod embed;
pub mod execute;
pub mod lock;
pub mod watch;
```

- [ ] **Step 9: Run the failing test — passing**

Run: `cargo test --test embed_sync embed_sync_no_push_pulls_into_tempdir`
Expected: 1 passed.

If the test errors with "env 'main' is not defined in rdc.toml", the toml seeding in Step 2 has wrong indentation — verify the heredoc.

- [ ] **Step 10: Commit**

```bash
git add src/cli/sync/embed.rs src/cli/sync/mod.rs src/cli/sync/watch.rs src/cli/mod.rs tests/embed_sync.rs
git commit -m "feat(rdc): add cli::sync::embed::sync_no_push for desktop-app embedders"
```

---

## Phase 5 — Sync orchestrator

### Task 10: Write `rdc.toml` reconciler

**Files:**
- Create: `rossum-local/src/rdc_toml.rs`
- Create: `rossum-local/tests/rdc_toml.rs`
- Modify: `rossum-local/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `rossum-local/tests/rdc_toml.rs`:

```rust
use rossum_local::rdc_toml::ensure_rdc_toml;

#[test]
fn writes_new_rdc_toml() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_rdc_toml(tmp.path(), "https://x.rossum.ai/api/v1", 42).unwrap();
    let body = std::fs::read_to_string(tmp.path().join("rdc.toml")).unwrap();
    assert!(body.contains("[envs.main]"));
    assert!(body.contains(r#"api_base = "https://x.rossum.ai/api/v1""#));
    assert!(body.contains("org_id = 42"));
}

#[test]
fn updates_existing_rdc_toml_when_api_base_changed() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_rdc_toml(tmp.path(), "https://old/api/v1", 42).unwrap();
    ensure_rdc_toml(tmp.path(), "https://new/api/v1", 42).unwrap();
    let body = std::fs::read_to_string(tmp.path().join("rdc.toml")).unwrap();
    assert!(body.contains("https://new/api/v1"));
    assert!(!body.contains("https://old/api/v1"));
}

#[test]
fn is_idempotent_when_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_rdc_toml(tmp.path(), "https://x/api/v1", 42).unwrap();
    let before = std::fs::metadata(tmp.path().join("rdc.toml")).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    ensure_rdc_toml(tmp.path(), "https://x/api/v1", 42).unwrap();
    let after = std::fs::metadata(tmp.path().join("rdc.toml")).unwrap().modified().unwrap();
    assert_eq!(before, after, "should not rewrite when content matches");
}
```

- [ ] **Step 2: Run — confirm fail**

Run: `cargo test --package rossum-local --test rdc_toml`
Expected: `unresolved import`.

- [ ] **Step 3: Implement**

Write `rossum-local/src/rdc_toml.rs`:

```rust
use anyhow::{Context, Result};
use std::path::Path;

/// Ensure `<cwd>/rdc.toml` declares `[envs.main]` with the given API
/// base and org id. Writes when missing or when the desired body
/// differs from the on-disk body; no-op when already in sync.
pub fn ensure_rdc_toml(cwd: &Path, api_base: &str, org_id: u64) -> Result<()> {
    std::fs::create_dir_all(cwd).with_context(|| format!("creating {}", cwd.display()))?;
    let path = cwd.join("rdc.toml");
    let desired = format!(
        "[envs.main]\napi_base = \"{}\"\norg_id = {}\n",
        api_base, org_id
    );
    let same = match std::fs::read_to_string(&path) {
        Ok(existing) => existing == desired,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if same {
        return Ok(());
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, desired.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
```

- [ ] **Step 4: Add to `lib.rs`**

```rust
pub mod rdc_toml;
```

(insert alphabetically among existing `pub mod` lines)

- [ ] **Step 5: Run tests — passing**

Run: `cargo test --package rossum-local --test rdc_toml`
Expected: 3 passed.

- [ ] **Step 6: Commit**

```bash
git add rossum-local/src/rdc_toml.rs rossum-local/src/lib.rs rossum-local/tests/rdc_toml.rs
git commit -m "feat(rossum-local): ensure_rdc_toml writer (creates/reconciles, idempotent)"
```

---

### Task 11: Sync orchestrator — happy-path wiring

**Files:**
- Create: `rossum-local/src/sync.rs`
- Create: `rossum-local/tests/sync_happy_path.rs`
- Modify: `rossum-local/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `rossum-local/tests/sync_happy_path.rs`:

```rust
use rossum_local::auth::TokenSource;
use rossum_local::connection::{AuthKind, Connection, ConnectionStatus};
use rossum_local::keychain::{fake::InMemoryKeychain, Keychain, TokenEntry};
use rossum_local::sync::run_sync;
use std::path::PathBuf;
use ulid::Ulid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn run_sync_writes_organization_json_and_index_md() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "name": "t", "url": format!("{}/organizations/1", server.uri())
        })))
        .mount(&server)
        .await;

    for kind in [
        "workspaces", "queues", "schemas", "inboxes", "hooks", "rules",
        "labels", "engines", "engine_fields", "workflows", "workflow_steps",
        "email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{}", kind)))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [], "pagination": {"next": null, "total_pages": 1, "total": 0}
            })))
            .mount(&server)
            .await;
    }

    let tmp = tempfile::tempdir().unwrap();
    let kc = InMemoryKeychain::default();
    let id = Ulid::new();
    kc.put_token(
        id,
        &TokenEntry {
            token: "t".into(),
            expires_at_unix: Some(i64::MAX),
        },
    )
    .unwrap();

    let conn = Connection {
        id,
        name: "t".into(),
        slug: "t".into(),
        api_base: server.uri(),
        org_id: 1,
        folder: tmp.path().join("t"),
        auth_kind: AuthKind::Token,
        last_sync_unix: None,
        last_status: ConnectionStatus::Never,
        file_count: 0,
    };

    let outcome = run_sync(&conn, &kc).await.unwrap();
    assert!(outcome.file_count >= 1);
    assert!(conn.folder.join("envs/main/organization.json").exists());
    assert!(conn.folder.join("envs/main/_index.md").exists());
    assert!(conn.folder.join("rdc.toml").exists());
}
```

- [ ] **Step 2: Run — confirm fail**

Run: `cargo test --package rossum-local --test sync_happy_path`
Expected: `unresolved import rossum_local::sync`.

- [ ] **Step 3: Implement**

Write `rossum-local/src/sync.rs`:

```rust
use crate::auth::{resolve_token_for_sync, ResolveError};
use crate::connection::Connection;
use crate::keychain::Keychain;
use crate::rdc_toml::ensure_rdc_toml;
use anyhow::{Context, Result};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub file_count: u64,
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Auth(#[from] ResolveError),
    #[error("Could not write to {0}. Make space and try again.")]
    DiskFull(String),
    #[error("Folder is in use by another rdc process. Try again.")]
    LockContended,
    #[error("{0}")]
    Other(String),
}

pub async fn run_sync<K: Keychain + ?Sized>(
    conn: &Connection,
    kc: &K,
) -> Result<SyncOutcome, SyncError> {
    let ts = resolve_token_for_sync(conn, kc).await?;
    ensure_rdc_toml(&conn.folder, &conn.api_base, conn.org_id)
        .map_err(|e| classify_io_err(e, &conn.folder))?;

    rdc::cli::sync::embed::sync_no_push(&conn.folder, "main", &ts.token)
        .await
        .map_err(|e| classify_rdc_err(e, &conn.folder))?;

    let file_count = count_snapshot_files(&conn.folder);
    Ok(SyncOutcome { file_count })
}

fn classify_io_err(e: anyhow::Error, folder: &std::path::Path) -> SyncError {
    let msg = format!("{e:#}");
    if msg.contains("No space left") {
        SyncError::DiskFull(folder.display().to_string())
    } else {
        SyncError::Other(msg)
    }
}

fn classify_rdc_err(e: anyhow::Error, folder: &std::path::Path) -> SyncError {
    let msg = format!("{e:#}");
    if msg.contains("lock") && msg.contains("contend") {
        SyncError::LockContended
    } else {
        classify_io_err(e, folder)
    }
}

fn count_snapshot_files(folder: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.file_name().map(|n| n == ".rdc").unwrap_or(false) {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    walk(&path, acc);
                } else if meta.is_file() {
                    *acc += 1;
                }
            }
        }
    }
    let mut n = 0;
    walk(&folder.join("envs/main"), &mut n);
    n
}
```

- [ ] **Step 4: Add to `lib.rs`**

```rust
pub mod sync;
```

- [ ] **Step 5: Run tests — passing**

Run: `cargo test --package rossum-local --test sync_happy_path`
Expected: 1 passed.

If the test fails with "env 'main' is not defined" the rdc-toml writer is not being invoked early enough — verify `ensure_rdc_toml` runs before `sync_no_push`.

- [ ] **Step 6: Commit**

```bash
git add rossum-local/src/sync.rs rossum-local/src/lib.rs rossum-local/tests/sync_happy_path.rs
git commit -m "feat(rossum-local): sync orchestrator wiring auth + rdc.toml + embed::sync_no_push"
```

---

### Task 12: Sync queue — single in-flight per Connection, global serialization in v1

**Files:**
- Create: `rossum-local/src/sync_queue.rs`
- Create: `rossum-local/tests/sync_queue.rs`
- Modify: `rossum-local/src/lib.rs`

This implements the spec's "one sync at a time per Connection" + "across Connections, multiple may run in parallel" — but bounded by a global concurrency limit of N (default 4) so the user can't accidentally fire 50 syncs at once.

- [ ] **Step 1: Write the failing test**

Create `rossum-local/tests/sync_queue.rs`:

```rust
use rossum_local::sync_queue::SyncQueue;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ulid::Ulid;

#[tokio::test]
async fn second_sync_for_same_connection_is_rejected_while_first_runs() {
    let q = SyncQueue::new(4);
    let id = Ulid::new();
    let started = Arc::new(AtomicUsize::new(0));

    let s2 = started.clone();
    let _h1 = q
        .submit(id, async move {
            s2.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(10)).await;
    let err = q
        .submit(id, async move {})
        .err()
        .expect("second submit rejected");
    assert!(format!("{err:#}").contains("already syncing"));
}

#[tokio::test]
async fn distinct_connections_run_in_parallel_up_to_limit() {
    let q = SyncQueue::new(2);
    let counter = Arc::new(AtomicUsize::new(0));

    let ids: Vec<_> = (0..2).map(|_| Ulid::new()).collect();
    let mut handles = Vec::new();
    for id in &ids {
        let c = counter.clone();
        handles.push(
            q.submit(*id, async move {
                c.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
            })
            .unwrap(),
        );
    }
    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}
```

- [ ] **Step 2: Run — confirm fail**

Run: `cargo test --package rossum-local --test sync_queue`
Expected: `unresolved import`.

- [ ] **Step 3: Implement**

Write `rossum-local/src/sync_queue.rs`:

```rust
use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;
use ulid::Ulid;

pub struct SyncQueue {
    in_flight: Arc<Mutex<HashSet<Ulid>>>,
    sem: Arc<Semaphore>,
}

impl SyncQueue {
    pub fn new(parallel_limit: usize) -> Self {
        Self {
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            sem: Arc::new(Semaphore::new(parallel_limit)),
        }
    }

    pub fn submit<F>(&self, id: Ulid, fut: F) -> Result<JoinHandle<()>>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let in_flight = self.in_flight.clone();
        let sem = self.sem.clone();

        // Synchronous claim of the in-flight slot so duplicate submissions
        // are rejected immediately without racing.
        {
            let mut guard = in_flight
                .try_lock()
                .map_err(|_| anyhow!("sync queue contended; try again"))?;
            if !guard.insert(id) {
                return Err(anyhow!("already syncing this Connection"));
            }
        }

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            fut.await;
            in_flight.lock().await.remove(&id);
        });
        Ok(handle)
    }
}
```

- [ ] **Step 4: Add to `lib.rs`**

```rust
pub mod sync_queue;
```

- [ ] **Step 5: Run — passing**

Run: `cargo test --package rossum-local --test sync_queue`
Expected: 2 passed.

- [ ] **Step 6: Commit**

```bash
git add rossum-local/src/sync_queue.rs rossum-local/src/lib.rs rossum-local/tests/sync_queue.rs
git commit -m "feat(rossum-local): SyncQueue with per-connection dedup + global parallel cap"
```

---

## Phase 6 — Tauri commands + app state

### Task 13: App state + Tauri command surface for the registry

**Files:**
- Create: `rossum-local/src/state.rs`
- Create: `rossum-local/src/commands.rs`
- Modify: `rossum-local/src/main.rs`
- Modify: `rossum-local/src/lib.rs`

- [ ] **Step 1: Implement `state.rs` — the singleton app state**

Write `rossum-local/src/state.rs`:

```rust
use crate::keychain::macos::MacOsKeychain;
use crate::registry::Registry;
use crate::settings::Settings;
use crate::sync_queue::SyncQueue;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct AppState {
    pub registry: Mutex<Registry>,
    pub settings: Mutex<Settings>,
    pub registry_path: PathBuf,
    pub settings_path: PathBuf,
    pub keychain: Arc<MacOsKeychain>,
    pub queue: SyncQueue,
}

impl AppState {
    pub fn load() -> Result<Self> {
        let registry_path = crate::paths::registry_path()?;
        let settings_path = crate::paths::settings_path()?;
        let registry = Registry::load(&registry_path)?;
        let settings = Settings::load(&settings_path)?;
        Ok(Self {
            registry: Mutex::new(registry),
            settings: Mutex::new(settings),
            registry_path,
            settings_path,
            keychain: Arc::new(MacOsKeychain),
            queue: SyncQueue::new(4),
        })
    }
}
```

- [ ] **Step 2: Implement `commands.rs` — read-only registry surface**

Write `rossum-local/src/commands.rs`:

```rust
use crate::connection::Connection;
use crate::state::AppState;
use serde::Serialize;
use tauri::State;

#[derive(Serialize)]
pub struct ConnectionSummary {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub api_base: String,
    pub org_id: u64,
    pub folder: String,
    pub auth_kind: String,
    pub last_sync_unix: Option<i64>,
    pub last_status: String,
    pub last_status_message: Option<String>,
    pub file_count: u64,
}

impl From<&Connection> for ConnectionSummary {
    fn from(c: &Connection) -> Self {
        use crate::connection::ConnectionStatus as S;
        let (status, msg) = match &c.last_status {
            S::Never => ("never", None),
            S::Ok => ("ok", None),
            S::Error(m) => ("error", Some(m.clone())),
        };
        Self {
            id: c.id.to_string(),
            name: c.name.clone(),
            slug: c.slug.clone(),
            api_base: c.api_base.clone(),
            org_id: c.org_id,
            folder: c.folder.display().to_string(),
            auth_kind: match c.auth_kind {
                crate::connection::AuthKind::Token => "token".into(),
                crate::connection::AuthKind::Password => "password".into(),
            },
            last_sync_unix: c.last_sync_unix,
            last_status: status.into(),
            last_status_message: msg,
            file_count: c.file_count,
        }
    }
}

#[tauri::command]
pub async fn list_connections(state: State<'_, AppState>) -> Result<Vec<ConnectionSummary>, String> {
    let reg = state.registry.lock().await;
    Ok(reg.connections().iter().map(ConnectionSummary::from).collect())
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<SettingsResponse, String> {
    let s = state.settings.lock().await;
    Ok(SettingsResponse {
        default_folder_parent: s.default_folder_parent.display().to_string(),
        update_channel: match s.update_channel {
            crate::settings::UpdateChannel::Stable => "stable".into(),
            crate::settings::UpdateChannel::Beta => "beta".into(),
        },
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

#[derive(Serialize)]
pub struct SettingsResponse {
    pub default_folder_parent: String,
    pub update_channel: String,
    pub app_version: String,
}
```

- [ ] **Step 3: Wire commands and state into `main.rs`**

Replace `rossum-local/src/main.rs` with:

```rust
use rossum_local::commands;
use rossum_local::state::AppState;

#[tokio::main]
async fn main() {
    let app_state = AppState::load().expect("loading app state");

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::list_connections,
            commands::get_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Rossum Local");
}
```

- [ ] **Step 4: Add to `lib.rs`**

```rust
pub mod commands;
pub mod state;
```

- [ ] **Step 5: Build**

Run: `cargo build --package rossum-local`
Expected: succeeds.

- [ ] **Step 6: Commit**

```bash
git add rossum-local/src/state.rs rossum-local/src/commands.rs rossum-local/src/main.rs rossum-local/src/lib.rs
git commit -m "feat(rossum-local): AppState + Tauri command surface for connections and settings"
```

---

### Task 14: `add_connection` command — validate + Keychain + registry

**Files:**
- Modify: `rossum-local/src/commands.rs`
- Create: `rossum-local/src/url_normalize.rs`
- Create: `rossum-local/tests/url_normalize.rs`
- Create: `rossum-local/tests/add_connection_validation.rs`
- Modify: `rossum-local/src/lib.rs`
- Modify: `rossum-local/src/main.rs`

- [ ] **Step 1: Write the URL-normalize test**

Create `rossum-local/tests/url_normalize.rs`:

```rust
use rossum_local::url_normalize::normalize_api_base;

#[test]
fn strips_trailing_slash() {
    assert_eq!(
        normalize_api_base("https://x.rossum.ai/api/v1/").unwrap(),
        "https://x.rossum.ai/api/v1"
    );
}

#[test]
fn appends_api_v1_when_missing() {
    assert_eq!(
        normalize_api_base("https://x.rossum.ai").unwrap(),
        "https://x.rossum.ai/api/v1"
    );
    assert_eq!(
        normalize_api_base("https://x.rossum.ai/").unwrap(),
        "https://x.rossum.ai/api/v1"
    );
}

#[test]
fn preserves_explicit_api_v1() {
    assert_eq!(
        normalize_api_base("https://x.rossum.ai/api/v1").unwrap(),
        "https://x.rossum.ai/api/v1"
    );
}

#[test]
fn rejects_non_http_scheme() {
    assert!(normalize_api_base("ftp://x.rossum.ai/api/v1").is_err());
}

#[test]
fn rejects_garbage() {
    assert!(normalize_api_base("not-a-url").is_err());
}
```

- [ ] **Step 2: Run — confirm fail**

Run: `cargo test --package rossum-local --test url_normalize`
Expected: `unresolved import`.

- [ ] **Step 3: Implement `url_normalize.rs`**

Write `rossum-local/src/url_normalize.rs`:

```rust
use anyhow::{anyhow, Result};

pub fn normalize_api_base(input: &str) -> Result<String> {
    let trimmed = input.trim();
    let url = url::Url::parse(trimmed)
        .map_err(|e| anyhow!("Not a valid URL: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(anyhow!("URL must use http or https"));
    }
    let path = url.path().trim_end_matches('/');
    let final_path = if path.is_empty() {
        "/api/v1"
    } else if path.ends_with("/api/v1") || path == "/api/v1" {
        path
    } else if path == "/api" {
        "/api/v1"
    } else {
        // path looks custom (e.g. /v1 alone, or something else); keep as-is.
        path
    };
    let host = url.host_str().ok_or_else(|| anyhow!("URL missing host"))?;
    let port = url
        .port()
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    Ok(format!("{}://{host}{port}{final_path}", url.scheme()))
}
```

- [ ] **Step 4: Run url-normalize test — passing**

Run: `cargo test --package rossum-local --test url_normalize`
Expected: 5 passed.

- [ ] **Step 5: Add `url_normalize` to `lib.rs`**

```rust
pub mod url_normalize;
```

- [ ] **Step 6: Write the add_connection integration test**

Create `rossum-local/tests/add_connection_validation.rs`:

```rust
use rossum_local::commands::AddConnectionInput;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn add_connection_rejects_invalid_token_with_401() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let input = AddConnectionInput {
        name: "X".into(),
        api_base: server.uri(),
        org_id: 1,
        auth_kind: "token".into(),
        token: Some("bad".into()),
        username: None,
        password: None,
        folder: None,
    };

    let err = rossum_local::commands::validate_add_input_against_rossum(&input)
        .await
        .unwrap_err();
    assert!(err.to_lowercase().contains("sign-in"));
}

#[tokio::test]
async fn add_connection_accepts_valid_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1, "name": "t", "url": format!("{}/organizations/1", server.uri())
        })))
        .mount(&server)
        .await;

    let input = AddConnectionInput {
        name: "X".into(),
        api_base: server.uri(),
        org_id: 1,
        auth_kind: "token".into(),
        token: Some("good".into()),
        username: None,
        password: None,
        folder: None,
    };

    let token = rossum_local::commands::validate_add_input_against_rossum(&input)
        .await
        .unwrap();
    assert_eq!(token, "good");
}
```

- [ ] **Step 7: Extend `commands.rs` — add the `AddConnectionInput`, validator, and `add_connection` command**

Append to `rossum-local/src/commands.rs`:

```rust
use crate::auth;
use crate::connection::{AuthKind, Connection, ConnectionStatus};
use crate::keychain::{Keychain, TokenEntry};
use crate::url_normalize::normalize_api_base;
use serde::Deserialize;
use std::path::PathBuf;
use ulid::Ulid;

#[derive(Debug, Clone, Deserialize)]
pub struct AddConnectionInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: String, // "token" | "password"
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub folder: Option<String>,
}

/// Validate the input against a live Rossum endpoint. Returns the bearer
/// token to store (either the user-pasted token or the token issued by
/// `/auth/login` for password mode). Errors are user-facing English
/// messages.
pub async fn validate_add_input_against_rossum(
    input: &AddConnectionInput,
) -> Result<String, String> {
    let api_base = normalize_api_base(&input.api_base).map_err(|e| e.to_string())?;
    let token = match input.auth_kind.as_str() {
        "token" => input
            .token
            .clone()
            .ok_or_else(|| "Token is required.".to_string())?,
        "password" => {
            let u = input.username.clone().ok_or_else(|| "Username is required.".to_string())?;
            let p = input.password.clone().ok_or_else(|| "Password is required.".to_string())?;
            auth::login(&api_base, &u, &p).await.map_err(|e| e.to_string())?
        }
        other => return Err(format!("Unknown auth_kind '{other}'.")),
    };

    let url = format!("{}/organizations/{}", api_base, input.org_id);
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("Couldn't reach Rossum: {e}"))?;
    match resp.status().as_u16() {
        200 => Ok(token),
        401 | 403 => Err("Sign-in failed. Check your token and try again.".into()),
        404 => Err(format!("Organization {} not found on this URL.", input.org_id)),
        s => Err(format!("Rossum returned {s}; try again later.")),
    }
}

#[tauri::command]
pub async fn add_connection(
    state: State<'_, AppState>,
    input: AddConnectionInput,
) -> Result<ConnectionSummary, String> {
    let api_base = normalize_api_base(&input.api_base).map_err(|e| e.to_string())?;
    let token = validate_add_input_against_rossum(&input).await?;

    let mut reg = state.registry.lock().await;
    let used = reg.used_slugs();
    let slug = crate::slug::derive_slug(&input.name, &used);
    let id = Ulid::new();

    let folder = match input.folder.clone() {
        Some(f) => PathBuf::from(f),
        None => state.settings.lock().await.default_folder_parent.join(&slug),
    };
    std::fs::create_dir_all(&folder).map_err(|e| format!("Creating folder: {e}"))?;

    let auth_kind = match input.auth_kind.as_str() {
        "token" => AuthKind::Token,
        "password" => AuthKind::Password,
        other => return Err(format!("Unknown auth_kind '{other}'.")),
    };

    let token_entry = match auth_kind {
        AuthKind::Token => TokenEntry { token, expires_at_unix: None },
        AuthKind::Password => TokenEntry {
            token,
            expires_at_unix: Some(now_unix() + 162 * 3600),
        },
    };
    state
        .keychain
        .put_token(id, &token_entry)
        .map_err(|e| format!("Keychain write: {e:#}"))?;
    if matches!(auth_kind, AuthKind::Password) {
        state
            .keychain
            .put_credentials(
                id,
                input.username.as_deref().unwrap(),
                input.password.as_deref().unwrap(),
            )
            .map_err(|e| format!("Keychain write: {e:#}"))?;
    }

    let conn = Connection {
        id,
        name: input.name.clone(),
        slug,
        api_base,
        org_id: input.org_id,
        folder,
        auth_kind,
        last_sync_unix: None,
        last_status: ConnectionStatus::Never,
        file_count: 0,
    };
    reg.upsert(conn.clone());
    reg.save(&state.registry_path)
        .map_err(|e| format!("Saving registry: {e:#}"))?;
    Ok((&conn).into())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
```

- [ ] **Step 8: Register the new command in `main.rs`**

Update the `invoke_handler` macro:

```rust
.invoke_handler(tauri::generate_handler![
    commands::list_connections,
    commands::get_settings,
    commands::add_connection,
])
```

- [ ] **Step 9: Run tests — passing**

Run: `cargo test --package rossum-local --test add_connection_validation`
Expected: 2 passed.

- [ ] **Step 10: Commit**

```bash
git add rossum-local/src/url_normalize.rs rossum-local/src/commands.rs rossum-local/src/lib.rs rossum-local/src/main.rs rossum-local/tests/url_normalize.rs rossum-local/tests/add_connection_validation.rs
git commit -m "feat(rossum-local): add_connection Tauri command with URL normalization + Keychain writes"
```

---

### Task 15: `sync_connection` command — wire SyncQueue + emit progress event

**Files:**
- Modify: `rossum-local/src/commands.rs`
- Modify: `rossum-local/src/main.rs`
- Create: `rossum-local/tests/sync_command.rs`

- [ ] **Step 1: Add the command implementation**

Append to `rossum-local/src/commands.rs`:

```rust
use crate::sync::{run_sync, SyncError};
use crate::connection::ConnectionStatus;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

#[derive(Serialize, Clone)]
pub struct SyncProgress {
    pub connection_id: String,
    pub phase: String, // "started" | "done" | "error"
    pub message: Option<String>,
    pub file_count: Option<u64>,
}

#[tauri::command]
pub async fn sync_connection(
    app: AppHandle,
    state: State<'_, AppState>,
    connection_id: String,
) -> Result<(), String> {
    let id: Ulid = connection_id.parse().map_err(|_| "Bad connection id".to_string())?;
    let conn = state
        .registry
        .lock()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| "Connection not found".to_string())?;

    let kc = state.keychain.clone();
    let registry_path = state.registry_path.clone();
    let registry = {
        // Take a clone of the Arc-equivalent so the spawned task can update.
        // tokio Mutex doesn't impl Clone; instead we re-lock in the task.
        ()
    };
    let _ = registry;

    let app2 = app.clone();
    let conn2 = conn.clone();

    let registry_arc: std::sync::Arc<tokio::sync::Mutex<crate::registry::Registry>> = {
        // Tauri State holds a `Mutex<Registry>`; we need an Arc to move into
        // the spawned task. The simplest way is to expose state via an
        // application-level Arc. See note below.
        std::sync::Arc::new(tokio::sync::Mutex::new(crate::registry::Registry::load(
            &registry_path,
        ).map_err(|e| format!("{e:#}"))?))
    };

    let _ = app.emit(
        "sync-progress",
        SyncProgress {
            connection_id: id.to_string(),
            phase: "started".into(),
            message: None,
            file_count: None,
        },
    );

    state
        .queue
        .submit(id, async move {
            let result = run_sync(&conn2, &*kc).await;
            // Update registry with outcome.
            let mut reg = registry_arc.lock().await;
            let mut new_conn = conn2.clone();
            match &result {
                Ok(outcome) => {
                    new_conn.last_sync_unix = Some(now_unix());
                    new_conn.last_status = ConnectionStatus::Ok;
                    new_conn.file_count = outcome.file_count;
                }
                Err(e) => {
                    new_conn.last_status = ConnectionStatus::Error(format!("{e}"));
                }
            }
            reg.upsert(new_conn);
            let _ = reg.save(&registry_path);

            let progress = match &result {
                Ok(o) => SyncProgress {
                    connection_id: id.to_string(),
                    phase: "done".into(),
                    message: None,
                    file_count: Some(o.file_count),
                },
                Err(e) => SyncProgress {
                    connection_id: id.to_string(),
                    phase: "error".into(),
                    message: Some(format!("{e}")),
                    file_count: None,
                },
            };
            let _ = app2.emit("sync-progress", progress);
        })
        .map_err(|e| format!("{e:#}"))?;

    Ok(())
}
```

**Note on the temporary `registry_arc`:** the above code reloads the registry from disk inside the spawned task to avoid a borrow-across-await issue with `State<'_, AppState>`. This works but re-reads the file. A cleaner approach is to refactor `AppState` so `registry` is `Arc<Mutex<Registry>>` directly; do that refactor here:

In `state.rs`, change:

```rust
pub registry: Mutex<Registry>,
```

to:

```rust
pub registry: std::sync::Arc<tokio::sync::Mutex<Registry>>,
```

and the constructor accordingly:

```rust
registry: std::sync::Arc::new(Mutex::new(registry)),
```

Update all `state.registry.lock().await` call sites — they keep working because `Arc<Mutex>::lock` works the same. Update `sync_connection` to clone the Arc and use it inside the task instead of the disk re-read. Replace the entire `let registry_arc: ... = { ... };` block with:

```rust
let registry_arc = state.registry.clone();
```

and remove the now-unused `registry_path` clone if not needed (it's still needed for `reg.save`).

- [ ] **Step 2: Apply the AppState refactor**

In `rossum-local/src/state.rs`:

```rust
use std::sync::Arc;
use tokio::sync::Mutex;
// ...
pub registry: Arc<Mutex<Registry>>,
pub settings: Arc<Mutex<Settings>>,
```

and constructor:

```rust
registry: Arc::new(Mutex::new(registry)),
settings: Arc::new(Mutex::new(settings)),
```

- [ ] **Step 3: Simplify `sync_connection`**

Replace the `registry_arc` block with `let registry_arc = state.registry.clone();`.

- [ ] **Step 4: Add `now_unix` helper if not already present**

Already added in Task 14 Step 7.

- [ ] **Step 5: Write a smoke test**

Create `rossum-local/tests/sync_command.rs`:

```rust
// The Tauri command itself requires a full Tauri AppHandle, which is
// awkward to construct in a unit test. We test the underlying
// `run_sync` already (Task 11). Here we just assert that the queue
// rejects double-submissions for the same Connection.

use rossum_local::sync_queue::SyncQueue;
use std::time::Duration;
use ulid::Ulid;

#[tokio::test]
async fn double_submit_rejected() {
    let q = SyncQueue::new(2);
    let id = Ulid::new();
    let _h = q
        .submit(id, async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .unwrap();
    let err = q.submit(id, async move {}).err().unwrap();
    assert!(format!("{err}").contains("already syncing"));
}
```

- [ ] **Step 6: Register the command in `main.rs`**

```rust
.invoke_handler(tauri::generate_handler![
    commands::list_connections,
    commands::get_settings,
    commands::add_connection,
    commands::sync_connection,
])
```

- [ ] **Step 7: Build + smoke test**

Run:
```
cargo build --package rossum-local
cargo test --package rossum-local --test sync_command
```
Expected: build succeeds; 1 passed.

- [ ] **Step 8: Commit**

```bash
git add rossum-local/src/commands.rs rossum-local/src/state.rs rossum-local/src/main.rs rossum-local/tests/sync_command.rs
git commit -m "feat(rossum-local): sync_connection Tauri command emits sync-progress events"
```

---

## Phase 7 — Frontend shell

### Task 16: HTML scaffold + CSS theme + empty-state view

**Files:**
- Modify: `rossum-local/ui/index.html`
- Create: `rossum-local/ui/styles.css`
- Create: `rossum-local/ui/main.ts`
- Create: `rossum-local/ui/tsconfig.json`

The frontend is vanilla TypeScript compiled by `tsc`. No bundler, no framework. Tauri serves `ui/` as the frontendDist.

- [ ] **Step 1: Write `tsconfig.json`**

`rossum-local/ui/tsconfig.json`:

```json
{
  "compilerOptions": {
    "target": "ES2020",
    "module": "ES2020",
    "moduleResolution": "node",
    "strict": true,
    "esModuleInterop": true,
    "skipLibCheck": true,
    "outDir": "./",
    "rootDir": "./",
    "lib": ["ES2020", "DOM"],
    "allowImportingTsExtensions": false
  },
  "include": ["*.ts"]
}
```

- [ ] **Step 2: Write `styles.css`**

`rossum-local/ui/styles.css`:

```css
:root {
  --bg: #ffffff;
  --bg-sidebar: #f4f4f5;
  --fg: #1a1a1a;
  --fg-muted: #6b7280;
  --accent: #ed8e47;
  --accent-fg: #ffffff;
  --border: #e5e7eb;
  --row-hover: #ececef;
  --row-selected: #e3e6ec;
  --success: #16a34a;
  --warning: #d97706;
  --error: #dc2626;
  --font: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Helvetica Neue", sans-serif;
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg: #1c1c1e;
    --bg-sidebar: #232325;
    --fg: #f5f5f7;
    --fg-muted: #a1a1aa;
    --border: #2c2c2e;
    --row-hover: #2a2a2d;
    --row-selected: #303034;
  }
}

* { box-sizing: border-box; }
html, body { margin: 0; padding: 0; height: 100%; font-family: var(--font); color: var(--fg); background: var(--bg); -webkit-font-smoothing: antialiased; }

.app { display: grid; grid-template-columns: 240px 1fr; height: 100vh; }
.sidebar { background: var(--bg-sidebar); border-right: 1px solid var(--border); display: flex; flex-direction: column; }
.sidebar-header { padding: 16px 16px 8px; font-size: 11px; font-weight: 600; letter-spacing: 0.06em; text-transform: uppercase; color: var(--fg-muted); }
.sidebar-list { flex: 1; overflow-y: auto; }
.sidebar-row { padding: 8px 16px; cursor: pointer; font-size: 13px; }
.sidebar-row:hover { background: var(--row-hover); }
.sidebar-row.selected { background: var(--row-selected); font-weight: 500; }
.sidebar-add { padding: 12px 16px; border-top: 1px solid var(--border); }
.btn { font-family: var(--font); font-size: 13px; padding: 6px 12px; border-radius: 6px; border: 1px solid var(--border); background: var(--bg); color: var(--fg); cursor: pointer; }
.btn:hover { background: var(--row-hover); }
.btn-primary { background: var(--accent); color: var(--accent-fg); border-color: var(--accent); font-weight: 500; }
.btn-primary:hover { filter: brightness(0.95); }
.btn-link { background: transparent; border: none; color: var(--accent); cursor: pointer; padding: 0; font-size: 13px; }
.btn-destructive { color: var(--error); border-color: var(--error); }

.detail { padding: 24px 32px; overflow-y: auto; }
.detail h2 { margin: 0 0 4px; font-size: 18px; font-weight: 600; }
.detail .subtitle { color: var(--fg-muted); font-size: 13px; margin-bottom: 24px; }
.detail .row { display: flex; gap: 8px; align-items: center; margin: 8px 0; font-size: 13px; }
.detail .label { color: var(--fg-muted); width: 100px; }
.status-ok { color: var(--success); }
.status-warning { color: var(--warning); }
.status-error { color: var(--error); }

.empty { display: flex; align-items: center; justify-content: center; height: 100vh; flex-direction: column; gap: 12px; text-align: center; padding: 0 32px; }
.empty h1 { font-size: 22px; font-weight: 600; margin: 0; }
.empty p { color: var(--fg-muted); margin: 0 0 16px; max-width: 360px; }

.modal-backdrop { position: fixed; inset: 0; background: rgba(0,0,0,0.4); display: flex; align-items: flex-start; justify-content: center; padding-top: 80px; z-index: 10; }
.modal { background: var(--bg); border-radius: 10px; padding: 24px; width: 460px; box-shadow: 0 10px 40px rgba(0,0,0,0.2); }
.modal h3 { margin: 0 0 16px; font-size: 16px; font-weight: 600; }
.field { display: flex; flex-direction: column; gap: 4px; margin-bottom: 12px; }
.field label { font-size: 12px; color: var(--fg-muted); }
.field input, .field select { font-family: var(--font); font-size: 13px; padding: 6px 10px; border: 1px solid var(--border); border-radius: 6px; background: var(--bg); color: var(--fg); }
.field-error { color: var(--error); font-size: 12px; margin-top: 4px; }
.modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
.banner { padding: 12px 16px; border-radius: 6px; margin-bottom: 16px; }
.banner-error { background: rgba(220, 38, 38, 0.1); color: var(--error); border: 1px solid var(--error); }
.progress { height: 4px; background: var(--border); border-radius: 2px; overflow: hidden; margin: 12px 0; }
.progress-bar { height: 100%; background: var(--accent); animation: indeterminate 1.4s ease-in-out infinite; width: 30%; }
@keyframes indeterminate {
  0% { margin-left: -30%; }
  100% { margin-left: 100%; }
}
```

- [ ] **Step 3: Write the empty-state and shell HTML**

Replace `rossum-local/ui/index.html`:

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <title>Rossum Local</title>
  <link rel="stylesheet" href="styles.css" />
  <meta http-equiv="Content-Security-Policy" content="default-src 'self'; style-src 'self' 'unsafe-inline'" />
</head>
<body>
  <div id="root"></div>
  <script type="module" src="main.ts"></script>
</body>
</html>
```

- [ ] **Step 4: Write the empty-state `main.ts`**

`rossum-local/ui/main.ts`:

```typescript
import { invoke } from "@tauri-apps/api/core";

interface ConnectionSummary {
  id: string;
  name: string;
  slug: string;
  api_base: string;
  org_id: number;
  folder: string;
  auth_kind: "token" | "password";
  last_sync_unix: number | null;
  last_status: "never" | "ok" | "error";
  last_status_message: string | null;
  file_count: number;
}

let connections: ConnectionSummary[] = [];
let selectedId: string | null = null;

async function load() {
  connections = await invoke<ConnectionSummary[]>("list_connections");
  if (connections.length > 0 && !selectedId) {
    selectedId = connections[0].id;
  }
  render();
}

function render() {
  const root = document.getElementById("root")!;
  if (connections.length === 0) {
    root.innerHTML = renderEmpty();
    document.getElementById("add-btn")!.onclick = () => openAddSheet();
    return;
  }
  root.innerHTML = `
    <div class="app">
      <aside class="sidebar">
        <div class="sidebar-header">Connections</div>
        <div class="sidebar-list" id="sidebar-list"></div>
        <div class="sidebar-add"><button class="btn" id="add-btn">+ Add Connection</button></div>
      </aside>
      <main class="detail" id="detail"></main>
    </div>
  `;
  renderSidebar();
  renderDetail();
  document.getElementById("add-btn")!.onclick = () => openAddSheet();
}

function renderEmpty(): string {
  return `
    <div class="empty">
      <h1>Sync a Rossum organization</h1>
      <p>Pull your Rossum config locally so Claude can read it.</p>
      <button class="btn btn-primary" id="add-btn">Add Connection</button>
    </div>
  `;
}

function renderSidebar() {
  const list = document.getElementById("sidebar-list")!;
  list.innerHTML = connections
    .map(
      (c) => `
      <div class="sidebar-row ${c.id === selectedId ? "selected" : ""}" data-id="${c.id}">
        ${escapeHtml(c.name)}
      </div>`,
    )
    .join("");
  list.querySelectorAll(".sidebar-row").forEach((row) => {
    row.addEventListener("click", () => {
      selectedId = (row as HTMLElement).dataset.id!;
      render();
    });
  });
}

function renderDetail() {
  const detail = document.getElementById("detail")!;
  const c = connections.find((c) => c.id === selectedId);
  if (!c) {
    detail.innerHTML = "";
    return;
  }
  detail.innerHTML = `
    <h2>${escapeHtml(c.name)}</h2>
    <div class="subtitle">${escapeHtml(c.api_base)}</div>
    <div class="row"><span class="label">Last synced</span><span>${formatLastSync(c.last_sync_unix)}</span></div>
    <div class="row"><span class="label">Status</span><span class="${statusClass(c.last_status)}">${formatStatus(c)}</span></div>
    <div class="row"><button class="btn btn-primary" id="sync-btn">Sync now</button></div>
    <div class="row"><span class="label">Folder</span><span>${escapeHtml(c.folder)}</span></div>
    <div class="row">
      <button class="btn" id="reveal-btn">Reveal in Finder</button>
      <button class="btn" id="copy-btn">Copy path</button>
    </div>
  `;
  // Wiring of sync/reveal/copy buttons happens in Task 17/19.
}

function formatLastSync(unix: number | null): string {
  if (unix === null) return "Never";
  const now = Math.floor(Date.now() / 1000);
  const delta = now - unix;
  if (delta < 60) return "just now";
  if (delta < 3600) return `${Math.floor(delta / 60)} min ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)} hr ago`;
  return `${Math.floor(delta / 86400)} day ago`;
}

function statusClass(s: string): string {
  if (s === "ok") return "status-ok";
  if (s === "error") return "status-error";
  return "";
}

function formatStatus(c: ConnectionSummary): string {
  if (c.last_status === "never") return "Not synced yet";
  if (c.last_status === "ok") return `Up to date · ${c.file_count} files`;
  return `Error: ${escapeHtml(c.last_status_message || "unknown")}`;
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function openAddSheet() {
  // Implemented in Task 17.
  console.log("openAddSheet — pending Task 17");
}

load();
```

- [ ] **Step 5: Compile TypeScript and verify**

Compile and verify:

```bash
cd rossum-local/ui
npx -y typescript@5 tsc -p tsconfig.json
```

Expected: `main.js` is generated alongside `main.ts`. The HTML refs `main.ts` but the script tag's `src="main.ts"` will be resolved as `main.js` in Tauri's WebView at runtime (the type-module loader needs the compiled output).

**Adjust HTML if Tauri doesn't auto-transform:** change the script tag to `src="main.js"` and re-run. Run `cargo tauri dev` to confirm.

- [ ] **Step 6: Update Tauri to use a compile step before bundling**

Edit `rossum-local/tauri.conf.json` to add a build step:

```json
"build": {
  "frontendDist": "ui",
  "devUrl": null,
  "beforeBuildCommand": "cd ui && npx -y typescript@5 tsc -p tsconfig.json",
  "beforeDevCommand": "cd ui && npx -y typescript@5 tsc -p tsconfig.json --watch"
}
```

- [ ] **Step 7: Smoke test**

```
cd rossum-local
cargo tauri dev
```

Expected: window opens showing the empty-state (since no Connections exist yet). Clicking "Add Connection" prints "openAddSheet — pending Task 17" to the console.

- [ ] **Step 8: Commit**

```bash
git add rossum-local/ui/ rossum-local/tauri.conf.json
git commit -m "feat(rossum-local): empty-state UI + sidebar/detail shell + light-dark CSS theme"
```

---

### Task 17: Add Connection sheet (modal with token/password toggle, URL normalization, validate-on-save)

**Files:**
- Modify: `rossum-local/ui/main.ts`

- [ ] **Step 1: Implement `openAddSheet` and `submitAddSheet`**

Replace the placeholder `openAddSheet` and add the submit logic. Append to `main.ts`:

```typescript
function openAddSheet() {
  const root = document.getElementById("root")!;
  const overlay = document.createElement("div");
  overlay.className = "modal-backdrop";
  overlay.innerHTML = `
    <div class="modal" role="dialog" aria-label="Add Connection">
      <h3>Add Connection</h3>
      <div id="add-error"></div>
      <div class="field"><label>Name</label><input id="add-name" type="text" placeholder="Acme Corp — Production" /></div>
      <div class="field"><label>API URL</label><input id="add-api" type="url" placeholder="https://acme.app.rossum.ai/api/v1" /></div>
      <div class="field"><label>Org ID</label><input id="add-org" type="number" min="1" /></div>
      <div class="field">
        <label>Sign in with</label>
        <select id="add-auth">
          <option value="password">Email + password</option>
          <option value="token">API token</option>
        </select>
      </div>
      <div id="auth-fields"></div>
      <div class="modal-actions">
        <button class="btn" id="add-cancel">Cancel</button>
        <button class="btn btn-primary" id="add-submit">Add &amp; Sync</button>
      </div>
    </div>
  `;
  root.appendChild(overlay);

  const authSel = document.getElementById("add-auth") as HTMLSelectElement;
  const renderAuthFields = () => {
    const c = document.getElementById("auth-fields")!;
    if (authSel.value === "token") {
      c.innerHTML = `<div class="field"><label>Token</label><input id="add-token" type="password" /></div>`;
    } else {
      c.innerHTML = `
        <div class="field"><label>Email</label><input id="add-username" type="email" /></div>
        <div class="field"><label>Password</label><input id="add-password" type="password" /></div>
      `;
    }
  };
  authSel.onchange = renderAuthFields;
  renderAuthFields();

  document.getElementById("add-cancel")!.onclick = () => overlay.remove();
  document.getElementById("add-submit")!.onclick = async () => {
    const errBox = document.getElementById("add-error")!;
    errBox.innerHTML = "";
    const input = {
      name: (document.getElementById("add-name") as HTMLInputElement).value.trim(),
      api_base: (document.getElementById("add-api") as HTMLInputElement).value.trim(),
      org_id: Number((document.getElementById("add-org") as HTMLInputElement).value),
      auth_kind: authSel.value,
      token: authSel.value === "token" ? (document.getElementById("add-token") as HTMLInputElement).value : null,
      username: authSel.value === "password" ? (document.getElementById("add-username") as HTMLInputElement).value : null,
      password: authSel.value === "password" ? (document.getElementById("add-password") as HTMLInputElement).value : null,
      folder: null,
    };
    if (!input.name || !input.api_base || !input.org_id) {
      errBox.innerHTML = `<div class="banner banner-error">Name, API URL, and Org ID are required.</div>`;
      return;
    }
    try {
      const created = await invoke<ConnectionSummary>("add_connection", { input });
      connections.push(created);
      selectedId = created.id;
      overlay.remove();
      render();
      // Trigger first sync immediately.
      await invoke("sync_connection", { connectionId: created.id });
    } catch (e) {
      errBox.innerHTML = `<div class="banner banner-error">${escapeHtml(String(e))}</div>`;
    }
  };
}
```

- [ ] **Step 2: Smoke test**

```
cd rossum-local
cargo tauri dev
```

In the running app, click "Add Connection", fill in a real Rossum URL + Org ID + token, and submit. Expected: the sheet closes, the sidebar shows the new Connection, and sync starts (but progress UI is wired in the next task).

If validation fails, the inline error banner appears.

- [ ] **Step 3: Commit**

```bash
git add rossum-local/ui/main.ts
git commit -m "feat(rossum-local-ui): Add Connection modal with token/password toggle"
```

---

## Phase 8 — Sync UI in detail pane

### Task 18: Sync button wiring + progress + error banner

**Files:**
- Modify: `rossum-local/ui/main.ts`

- [ ] **Step 1: Listen for `sync-progress` events and update the detail pane**

Append to `main.ts`:

```typescript
import { listen } from "@tauri-apps/api/event";

interface SyncProgressEvent {
  connection_id: string;
  phase: "started" | "done" | "error";
  message: string | null;
  file_count: number | null;
}

const syncState = new Map<string, "idle" | "running" | "error">();

async function setupEventListener() {
  await listen<SyncProgressEvent>("sync-progress", (e) => {
    const p = e.payload;
    if (p.phase === "started") {
      syncState.set(p.connection_id, "running");
    } else if (p.phase === "done") {
      syncState.set(p.connection_id, "idle");
      // Refresh from backend so last_sync_unix/file_count update.
      void load();
      return;
    } else if (p.phase === "error") {
      syncState.set(p.connection_id, "error");
      const c = connections.find((c) => c.id === p.connection_id);
      if (c) {
        c.last_status = "error";
        c.last_status_message = p.message ?? "Sync failed";
      }
    }
    if (selectedId === p.connection_id) renderDetail();
  });
}
```

Update `renderDetail()` to render based on `syncState`:

```typescript
function renderDetail() {
  const detail = document.getElementById("detail")!;
  const c = connections.find((c) => c.id === selectedId);
  if (!c) {
    detail.innerHTML = "";
    return;
  }
  const state = syncState.get(c.id) ?? "idle";
  detail.innerHTML = `
    <h2>${escapeHtml(c.name)}</h2>
    <div class="subtitle">${escapeHtml(c.api_base)}</div>
    <div class="row"><span class="label">Last synced</span><span>${formatLastSync(c.last_sync_unix)}</span></div>
    <div class="row"><span class="label">Status</span><span class="${statusClass(c.last_status)}">${formatStatus(c)}</span></div>
    ${state === "running"
      ? `<div class="progress"><div class="progress-bar"></div></div>`
      : `<div class="row"><button class="btn btn-primary" id="sync-btn">Sync now</button></div>`}
    ${state === "error" && c.last_status_message
      ? `<div class="banner banner-error">${escapeHtml(c.last_status_message)}</div>`
      : ""}
    <div class="row"><span class="label">Folder</span><span>${escapeHtml(c.folder)}</span></div>
    <div class="row">
      <button class="btn" id="reveal-btn">Reveal in Finder</button>
      <button class="btn" id="copy-btn">Copy path</button>
    </div>
    <div class="row" style="margin-top:32px;">
      <button class="btn-link" id="edit-creds-btn">Edit credentials</button>
      <span style="flex:1"></span>
      <button class="btn btn-destructive" id="remove-btn">Remove…</button>
    </div>
  `;
  const syncBtn = document.getElementById("sync-btn");
  if (syncBtn) {
    syncBtn.onclick = async () => {
      try {
        await invoke("sync_connection", { connectionId: c.id });
      } catch (e) {
        c.last_status = "error";
        c.last_status_message = String(e);
        renderDetail();
      }
    };
  }
  // Reveal / Copy / Edit / Remove wired in Tasks 19-20.
}
```

- [ ] **Step 2: Call the listener once on startup**

In `load()`, after the connections fetch:

```typescript
async function load() {
  connections = await invoke<ConnectionSummary[]>("list_connections");
  if (connections.length > 0 && !selectedId) {
    selectedId = connections[0].id;
  }
  render();
  await setupEventListener();
}
```

Move the `await setupEventListener()` to only run once. To ensure it runs exactly once, lift it out:

```typescript
let listenerAttached = false;

async function load() {
  connections = await invoke<ConnectionSummary[]>("list_connections");
  if (connections.length > 0 && !selectedId) {
    selectedId = connections[0].id;
  }
  if (!listenerAttached) {
    await setupEventListener();
    listenerAttached = true;
  }
  render();
}
```

- [ ] **Step 3: Smoke test the sync flow**

```
cd rossum-local
cargo tauri dev
```

Add a Connection, then click "Sync now". Expected: the button is replaced by an indeterminate progress bar; on completion, file count updates and the bar disappears. On error, the red banner shows the message.

- [ ] **Step 4: Commit**

```bash
git add rossum-local/ui/main.ts
git commit -m "feat(rossum-local-ui): Sync button + progress bar + error banner"
```

---

## Phase 9 — Edit credentials, Remove, Folder actions

### Task 19: Reveal in Finder + Copy path

**Files:**
- Modify: `rossum-local/ui/main.ts`
- Modify: `rossum-local/tauri.conf.json` (allowlist for shell + clipboard)

- [ ] **Step 1: Allowlist the required Tauri plugins**

In `rossum-local/tauri.conf.json`, ensure plugins are configured:

```json
"plugins": {
  "shell": {
    "open": true
  }
}
```

(Tauri 2's clipboard plugin needs no allowlist entry beyond initialization.)

- [ ] **Step 2: Add capabilities file**

Create `rossum-local/capabilities/default.json`:

```json
{
  "$schema": "https://schema.tauri.app/config/2",
  "identifier": "default",
  "description": "Default capabilities for Rossum Local",
  "windows": ["main"],
  "permissions": [
    "core:default",
    "shell:allow-open",
    "clipboard-manager:allow-write-text",
    "dialog:default"
  ]
}
```

Reference this in `tauri.conf.json` under `app.security`:

```json
"security": {
  "csp": "default-src 'self'; style-src 'self' 'unsafe-inline'",
  "capabilities": ["default"]
}
```

- [ ] **Step 3: Wire the buttons in `main.ts`**

Append to `renderDetail()` after the sync button wiring:

```typescript
const revealBtn = document.getElementById("reveal-btn");
if (revealBtn) {
  revealBtn.onclick = async () => {
    const { open } = await import("@tauri-apps/plugin-shell");
    await open(c.folder);
  };
}
const copyBtn = document.getElementById("copy-btn");
if (copyBtn) {
  copyBtn.onclick = async () => {
    const { writeText } = await import("@tauri-apps/plugin-clipboard-manager");
    await writeText(c.folder);
    copyBtn.textContent = "Copied!";
    setTimeout(() => { copyBtn.textContent = "Copy path"; }, 1200);
  };
}
```

- [ ] **Step 4: Smoke test**

```
cargo tauri dev
```

Click "Reveal in Finder" — Finder opens at the folder. Click "Copy path" — the button briefly shows "Copied!"; paste elsewhere to verify the path.

- [ ] **Step 5: Commit**

```bash
git add rossum-local/ui/main.ts rossum-local/tauri.conf.json rossum-local/capabilities/
git commit -m "feat(rossum-local): Reveal in Finder + Copy path actions"
```

---

### Task 20: Edit credentials + Remove Connection (Trash folder)

**Files:**
- Modify: `rossum-local/src/commands.rs`
- Modify: `rossum-local/src/main.rs`
- Create: `rossum-local/src/folder.rs`
- Modify: `rossum-local/ui/main.ts`

- [ ] **Step 1: Implement `folder.rs` — move-to-Trash on macOS**

Write `rossum-local/src/folder.rs`:

```rust
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

/// Move the given path to the user's Trash via `osascript` (Finder
/// `move ... to trash`). This is the public-API macOS pattern that
/// preserves "Put Back" recoverability — Finder records the original
/// location.
#[cfg(target_os = "macos")]
pub fn trash_folder(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let p = path
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 path: {}", path.display()))?;
    let script = format!(
        r#"tell application "Finder" to move POSIX file "{}" to trash"#,
        p.replace('"', r#"\""#)
    );
    let out = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .context("running osascript")?;
    if !out.status.success() {
        return Err(anyhow!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn trash_folder(path: &Path) -> Result<()> {
    // Linux/Windows builds (future). For now, fall back to a permanent
    // delete with explicit logging — the caller is responsible for
    // confirming with the user.
    if path.exists() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}
```

- [ ] **Step 2: Implement `edit_credentials` and `remove_connection` commands**

Append to `rossum-local/src/commands.rs`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct EditCredentialsInput {
    pub connection_id: String,
    pub auth_kind: String, // "token" | "password"
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[tauri::command]
pub async fn edit_credentials(
    state: State<'_, AppState>,
    input: EditCredentialsInput,
) -> Result<(), String> {
    let id: Ulid = input.connection_id.parse().map_err(|_| "Bad id".to_string())?;
    let mut reg = state.registry.lock().await;
    let mut conn = reg
        .get(id)
        .cloned()
        .ok_or_else(|| "Connection not found".to_string())?;

    let validation_input = AddConnectionInput {
        name: conn.name.clone(),
        api_base: conn.api_base.clone(),
        org_id: conn.org_id,
        auth_kind: input.auth_kind.clone(),
        token: input.token.clone(),
        username: input.username.clone(),
        password: input.password.clone(),
        folder: Some(conn.folder.display().to_string()),
    };
    let new_token = validate_add_input_against_rossum(&validation_input).await?;

    let new_kind = match input.auth_kind.as_str() {
        "token" => AuthKind::Token,
        "password" => AuthKind::Password,
        other => return Err(format!("Unknown auth_kind '{other}'.")),
    };
    let entry = match new_kind {
        AuthKind::Token => TokenEntry { token: new_token, expires_at_unix: None },
        AuthKind::Password => TokenEntry {
            token: new_token,
            expires_at_unix: Some(now_unix() + 162 * 3600),
        },
    };
    state
        .keychain
        .delete_all(id)
        .map_err(|e| format!("Clearing old credentials: {e:#}"))?;
    state
        .keychain
        .put_token(id, &entry)
        .map_err(|e| format!("Writing token: {e:#}"))?;
    if matches!(new_kind, AuthKind::Password) {
        state
            .keychain
            .put_credentials(
                id,
                input.username.as_deref().unwrap(),
                input.password.as_deref().unwrap(),
            )
            .map_err(|e| format!("Writing credentials: {e:#}"))?;
    }
    conn.auth_kind = new_kind;
    reg.upsert(conn);
    reg.save(&state.registry_path).map_err(|e| format!("{e:#}"))?;
    Ok(())
}

#[tauri::command]
pub async fn remove_connection(
    state: State<'_, AppState>,
    connection_id: String,
) -> Result<(), String> {
    let id: Ulid = connection_id.parse().map_err(|_| "Bad id".to_string())?;
    let mut reg = state.registry.lock().await;
    let conn = reg
        .remove(id)
        .ok_or_else(|| "Connection not found".to_string())?;
    state
        .keychain
        .delete_all(id)
        .map_err(|e| format!("Clearing Keychain: {e:#}"))?;
    crate::folder::trash_folder(&conn.folder).map_err(|e| format!("Moving folder to Trash: {e:#}"))?;
    reg.save(&state.registry_path).map_err(|e| format!("{e:#}"))?;
    Ok(())
}
```

- [ ] **Step 3: Add `folder` to `lib.rs` and the commands to `main.rs`**

`lib.rs`:

```rust
pub mod folder;
```

`main.rs` handler list:

```rust
.invoke_handler(tauri::generate_handler![
    commands::list_connections,
    commands::get_settings,
    commands::add_connection,
    commands::sync_connection,
    commands::edit_credentials,
    commands::remove_connection,
])
```

- [ ] **Step 4: Wire Edit + Remove buttons in `main.ts`**

Add helper functions and event wiring. Append to `main.ts`:

```typescript
function openEditCredentialsSheet(c: ConnectionSummary) {
  const root = document.getElementById("root")!;
  const overlay = document.createElement("div");
  overlay.className = "modal-backdrop";
  overlay.innerHTML = `
    <div class="modal">
      <h3>Edit credentials for ${escapeHtml(c.name)}</h3>
      <div id="edit-error"></div>
      <div class="field">
        <label>Sign in with</label>
        <select id="edit-auth">
          <option value="password" ${c.auth_kind === "password" ? "selected" : ""}>Email + password</option>
          <option value="token" ${c.auth_kind === "token" ? "selected" : ""}>API token</option>
        </select>
      </div>
      <div id="edit-fields"></div>
      <div class="modal-actions">
        <button class="btn" id="edit-cancel">Cancel</button>
        <button class="btn btn-primary" id="edit-save">Save</button>
      </div>
    </div>
  `;
  root.appendChild(overlay);
  const authSel = document.getElementById("edit-auth") as HTMLSelectElement;
  const renderEditFields = () => {
    const c = document.getElementById("edit-fields")!;
    if (authSel.value === "token") {
      c.innerHTML = `<div class="field"><label>New token</label><input id="edit-token" type="password" placeholder="Enter new token" /></div>`;
    } else {
      c.innerHTML = `
        <div class="field"><label>Email</label><input id="edit-username" type="email" /></div>
        <div class="field"><label>New password</label><input id="edit-password" type="password" placeholder="Enter new password" /></div>
      `;
    }
  };
  authSel.onchange = renderEditFields;
  renderEditFields();

  document.getElementById("edit-cancel")!.onclick = () => overlay.remove();
  document.getElementById("edit-save")!.onclick = async () => {
    const errBox = document.getElementById("edit-error")!;
    errBox.innerHTML = "";
    const input = {
      connection_id: c.id,
      auth_kind: authSel.value,
      token: authSel.value === "token" ? (document.getElementById("edit-token") as HTMLInputElement).value : null,
      username: authSel.value === "password" ? (document.getElementById("edit-username") as HTMLInputElement).value : null,
      password: authSel.value === "password" ? (document.getElementById("edit-password") as HTMLInputElement).value : null,
    };
    try {
      await invoke("edit_credentials", { input });
      overlay.remove();
      await load();
    } catch (e) {
      errBox.innerHTML = `<div class="banner banner-error">${escapeHtml(String(e))}</div>`;
    }
  };
}

function openRemoveSheet(c: ConnectionSummary) {
  const root = document.getElementById("root")!;
  const overlay = document.createElement("div");
  overlay.className = "modal-backdrop";
  overlay.innerHTML = `
    <div class="modal">
      <h3>Remove "${escapeHtml(c.name)}"?</h3>
      <p>This will delete the local folder (<code>${escapeHtml(c.folder)}</code>) and remove the stored sign-in. Rossum data is not affected.</p>
      <div class="modal-actions">
        <button class="btn" id="remove-cancel">Cancel</button>
        <button class="btn btn-destructive" id="remove-confirm">Remove</button>
      </div>
    </div>
  `;
  root.appendChild(overlay);
  document.getElementById("remove-cancel")!.onclick = () => overlay.remove();
  document.getElementById("remove-confirm")!.onclick = async () => {
    try {
      await invoke("remove_connection", { connectionId: c.id });
      overlay.remove();
      selectedId = null;
      await load();
    } catch (e) {
      alert(String(e));
    }
  };
}
```

Wire from `renderDetail()`:

```typescript
const editBtn = document.getElementById("edit-creds-btn");
if (editBtn) editBtn.onclick = () => openEditCredentialsSheet(c);
const removeBtn = document.getElementById("remove-btn");
if (removeBtn) removeBtn.onclick = () => openRemoveSheet(c);
```

- [ ] **Step 5: Smoke test**

```
cargo tauri dev
```

Test:
1. Click Edit credentials → change auth mode → save → verify it still syncs.
2. Click Remove… → confirm → folder appears in macOS Trash (open Trash to verify).

- [ ] **Step 6: Commit**

```bash
git add rossum-local/src/folder.rs rossum-local/src/commands.rs rossum-local/src/lib.rs rossum-local/src/main.rs rossum-local/ui/main.ts
git commit -m "feat(rossum-local): edit_credentials + remove_connection (Trash folder via osascript)"
```

---

## Phase 10 — Settings + Diagnostics windows

### Task 21: Settings window — default folder parent + update channel

**Files:**
- Modify: `rossum-local/src/commands.rs`
- Modify: `rossum-local/src/main.rs`
- Modify: `rossum-local/ui/main.ts`
- Modify: `rossum-local/tauri.conf.json`

- [ ] **Step 1: Add `update_settings` Tauri command**

Append to `rossum-local/src/commands.rs`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateSettingsInput {
    pub default_folder_parent: String,
    pub update_channel: String, // "stable" | "beta"
}

#[tauri::command]
pub async fn update_settings(
    state: State<'_, AppState>,
    input: UpdateSettingsInput,
) -> Result<(), String> {
    let mut s = state.settings.lock().await;
    s.default_folder_parent = std::path::PathBuf::from(&input.default_folder_parent);
    s.update_channel = match input.update_channel.as_str() {
        "stable" => crate::settings::UpdateChannel::Stable,
        "beta" => crate::settings::UpdateChannel::Beta,
        other => return Err(format!("Unknown channel '{other}'.")),
    };
    s.save(&state.settings_path).map_err(|e| format!("{e:#}"))?;
    Ok(())
}
```

Add to `main.rs` handler list:

```rust
commands::update_settings,
```

- [ ] **Step 2: Add a Settings menu item**

Tauri 2 uses the macOS menu builder. In `rossum-local/src/main.rs`, build a menu before `tauri::Builder::default()`:

```rust
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};

// ...inside main(), before tauri::Builder:
fn build_menu(app: &tauri::App) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let settings = MenuItemBuilder::new("Settings…")
        .id("open-settings")
        .accelerator("Cmd+,")
        .build(app)?;
    let diagnostics = MenuItemBuilder::new("Diagnostics…")
        .id("open-diagnostics")
        .build(app)?;
    let app_menu = SubmenuBuilder::new(app, "Rossum Local")
        .item(&settings)
        .separator()
        .item(&diagnostics)
        .separator()
        .quit()
        .build()?;
    let menu = MenuBuilder::new(app).item(&app_menu).build()?;
    Ok(menu)
}
```

In the Tauri builder:

```rust
.setup(|app| {
    let menu = build_menu(&app.handle()).expect("building menu");
    app.set_menu(menu)?;
    Ok(())
})
.on_menu_event(|app, event| match event.id().as_ref() {
    "open-settings" => {
        let _ = app.emit("open-settings", ());
    }
    "open-diagnostics" => {
        let _ = app.emit("open-diagnostics", ());
    }
    _ => {}
})
```

- [ ] **Step 3: Implement the Settings sheet on the frontend**

Append to `ui/main.ts`:

```typescript
async function openSettingsSheet() {
  const s = await invoke<{
    default_folder_parent: string;
    update_channel: string;
    app_version: string;
  }>("get_settings");
  const root = document.getElementById("root")!;
  const overlay = document.createElement("div");
  overlay.className = "modal-backdrop";
  overlay.innerHTML = `
    <div class="modal">
      <h3>Settings</h3>
      <div class="field">
        <label>Default folder location</label>
        <input id="setting-folder" type="text" value="${escapeHtml(s.default_folder_parent)}" />
      </div>
      <div class="field">
        <label>Update channel</label>
        <select id="setting-channel">
          <option value="stable" ${s.update_channel === "stable" ? "selected" : ""}>Stable</option>
          <option value="beta" ${s.update_channel === "beta" ? "selected" : ""}>Beta</option>
        </select>
      </div>
      <div class="field"><label>App version</label><div>${escapeHtml(s.app_version)}</div></div>
      <div class="modal-actions">
        <button class="btn" id="settings-cancel">Cancel</button>
        <button class="btn btn-primary" id="settings-save">Save</button>
      </div>
    </div>
  `;
  root.appendChild(overlay);
  document.getElementById("settings-cancel")!.onclick = () => overlay.remove();
  document.getElementById("settings-save")!.onclick = async () => {
    const input = {
      default_folder_parent: (document.getElementById("setting-folder") as HTMLInputElement).value,
      update_channel: (document.getElementById("setting-channel") as HTMLSelectElement).value,
    };
    try {
      await invoke("update_settings", { input });
      overlay.remove();
    } catch (e) {
      alert(String(e));
    }
  };
}

await listen("open-settings", () => openSettingsSheet());
```

- [ ] **Step 4: Smoke test**

```
cargo tauri dev
```

Press Cmd-, or use the menu: Rossum Local → Settings… A modal appears. Changing the folder + saving persists to `settings.json`.

- [ ] **Step 5: Commit**

```bash
git add rossum-local/src/commands.rs rossum-local/src/main.rs rossum-local/ui/main.ts rossum-local/tauri.conf.json
git commit -m "feat(rossum-local): Settings window with default folder + update channel"
```

---

### Task 22: Diagnostics window — version info + ring-buffer log + Copy

**Files:**
- Create: `rossum-local/src/diagnostics.rs`
- Modify: `rossum-local/src/commands.rs`
- Modify: `rossum-local/src/main.rs`
- Modify: `rossum-local/src/lib.rs`
- Modify: `rossum-local/ui/main.ts`

- [ ] **Step 1: Implement a small ring-buffer log**

Write `rossum-local/src/diagnostics.rs`:

```rust
use std::collections::VecDeque;
use std::sync::Mutex;

const CAP: usize = 100;

pub struct DiagLog {
    entries: Mutex<VecDeque<String>>,
}

impl Default for DiagLog {
    fn default() -> Self {
        Self { entries: Mutex::new(VecDeque::with_capacity(CAP)) }
    }
}

impl DiagLog {
    pub fn push(&self, line: impl Into<String>) {
        let mut q = self.entries.lock().unwrap();
        if q.len() == CAP {
            q.pop_front();
        }
        q.push_back(line.into());
    }

    pub fn snapshot(&self) -> Vec<String> {
        let q = self.entries.lock().unwrap();
        q.iter().cloned().collect()
    }
}
```

- [ ] **Step 2: Add DiagLog to AppState**

Modify `state.rs`:

```rust
use crate::diagnostics::DiagLog;
// ...
pub diag: Arc<DiagLog>,
// constructor:
diag: Arc::new(DiagLog::default()),
```

- [ ] **Step 3: Implement `get_diagnostics` command**

Append to `commands.rs`:

```rust
#[derive(Serialize)]
pub struct DiagnosticsResponse {
    pub app_version: String,
    pub rdc_version: String,
    pub os_version: String,
    pub connection_count: usize,
    pub log_lines: Vec<String>,
}

#[tauri::command]
pub async fn get_diagnostics(state: State<'_, AppState>) -> Result<DiagnosticsResponse, String> {
    let reg = state.registry.lock().await;
    Ok(DiagnosticsResponse {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        rdc_version: rdc::version().unwrap_or("unknown").to_string(),
        os_version: os_version_string(),
        connection_count: reg.connections().len(),
        log_lines: state.diag.snapshot(),
    })
}

fn os_version_string() -> String {
    // Best-effort; on macOS, `sw_vers -productVersion` is canonical.
    match std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
    {
        Ok(o) if o.status.success() => format!(
            "macOS {}",
            String::from_utf8_lossy(&o.stdout).trim()
        ),
        _ => "unknown".into(),
    }
}
```

The `rdc::version()` call assumes a small helper exists. Add `pub fn version() -> Option<&'static str> { option_env!("CARGO_PKG_VERSION") }` to rdc's `lib.rs` if not present (a 3-line addition).

- [ ] **Step 4: Wire the ring-buffer into sync events**

In `sync_connection` (commands.rs), before submitting and on completion:

```rust
state.diag.push(format!("sync start: {}", conn.name));
// ...
// in the spawned task, after run_sync result:
let msg = match &result {
    Ok(o) => format!("sync done: {} ({} files)", conn2.name, o.file_count),
    Err(e) => format!("sync error: {}: {}", conn2.name, e),
};
// Need a clone of state.diag accessible in the task:
```

To make `state.diag` accessible in the spawned task, clone the `Arc<DiagLog>` before `state.queue.submit`:

```rust
let diag = state.diag.clone();
// ...inside async move:
diag.push(msg);
```

- [ ] **Step 5: Add command + menu wiring**

`main.rs` handler list:

```rust
commands::get_diagnostics,
```

The menu already emits `open-diagnostics` from Task 21.

- [ ] **Step 6: Implement frontend Diagnostics sheet**

Append to `ui/main.ts`:

```typescript
async function openDiagnosticsSheet() {
  const d = await invoke<{
    app_version: string;
    rdc_version: string;
    os_version: string;
    connection_count: number;
    log_lines: string[];
  }>("get_diagnostics");
  const text = [
    `App: Rossum Local ${d.app_version}`,
    `rdc: ${d.rdc_version}`,
    `OS: ${d.os_version}`,
    `Connections: ${d.connection_count}`,
    "",
    "Recent log:",
    ...d.log_lines,
  ].join("\n");

  const root = document.getElementById("root")!;
  const overlay = document.createElement("div");
  overlay.className = "modal-backdrop";
  overlay.innerHTML = `
    <div class="modal" style="width: 560px;">
      <h3>Diagnostics</h3>
      <pre style="max-height: 320px; overflow: auto; font-size: 11px; background: var(--bg-sidebar); padding: 12px; border-radius: 6px;">${escapeHtml(text)}</pre>
      <div class="modal-actions">
        <button class="btn" id="diag-close">Close</button>
        <button class="btn btn-primary" id="diag-copy">Copy</button>
      </div>
    </div>
  `;
  root.appendChild(overlay);
  document.getElementById("diag-close")!.onclick = () => overlay.remove();
  document.getElementById("diag-copy")!.onclick = async () => {
    const { writeText } = await import("@tauri-apps/plugin-clipboard-manager");
    await writeText(text);
    (document.getElementById("diag-copy") as HTMLButtonElement).textContent = "Copied!";
  };
}

await listen("open-diagnostics", () => openDiagnosticsSheet());
```

- [ ] **Step 7: Add to `lib.rs`**

```rust
pub mod diagnostics;
```

- [ ] **Step 8: Smoke test**

```
cargo tauri dev
```

Menu: Rossum Local → Diagnostics… A modal shows app version, rdc version, OS version, Connection count, recent log. Copy works.

- [ ] **Step 9: Commit**

```bash
git add rossum-local/src/diagnostics.rs rossum-local/src/state.rs rossum-local/src/commands.rs rossum-local/src/main.rs rossum-local/src/lib.rs rossum-local/ui/main.ts src/lib.rs
git commit -m "feat(rossum-local): Diagnostics window with version info + ring-buffer log + copy"
```

---

## Phase 11 — Distribution

### Task 23: Tauri updater plugin wiring (Ed25519 + GitHub Releases feed)

**Files:**
- Modify: `rossum-local/Cargo.toml`
- Modify: `rossum-local/src/main.rs`
- Modify: `rossum-local/tauri.conf.json`
- Modify: `rossum-local/capabilities/default.json`

- [ ] **Step 1: Generate the updater key pair**

```bash
cd rossum-local
cargo tauri signer generate -w ~/.tauri/rossum-local.key
```

Expected: prints the public key. Save it for the next step. The private key file goes into `~/.tauri/rossum-local.key` and **never** lands in git.

- [ ] **Step 2: Add updater config to `tauri.conf.json`**

```json
"plugins": {
  "shell": { "open": true },
  "updater": {
    "active": true,
    "endpoints": [
      "https://github.com/mrtnzlml/rdc/releases/latest/download/rossum-local-latest.json"
    ],
    "pubkey": "<PASTE THE PUBLIC KEY FROM STEP 1>",
    "dialog": true
  }
}
```

- [ ] **Step 3: Initialize the updater plugin**

In `rossum-local/src/main.rs`, add:

```rust
.plugin(tauri_plugin_updater::Builder::new().build())
```

to the Builder chain (after `tauri_plugin_dialog::init()`).

- [ ] **Step 4: Add the updater permission**

In `rossum-local/capabilities/default.json`, extend `permissions`:

```json
"permissions": [
  "core:default",
  "shell:allow-open",
  "clipboard-manager:allow-write-text",
  "dialog:default",
  "updater:default"
]
```

- [ ] **Step 5: Build, do not test in dev**

Tauri's updater is normally a no-op in dev. Just verify it compiles:

```bash
cargo build --package rossum-local
```

- [ ] **Step 6: Commit**

```bash
git add rossum-local/src/main.rs rossum-local/tauri.conf.json rossum-local/capabilities/default.json rossum-local/Cargo.toml
git commit -m "feat(rossum-local): wire Tauri updater plugin (GitHub Releases feed, Ed25519 verify)"
```

---

### Task 24: GitHub Actions workflow — sign + notarize + DMG on tag

**Files:**
- Create: `.github/workflows/desktop-release.yml`

This workflow runs on `desktop-v*` tags and produces a signed, notarized, stapled `.dmg`. Secrets required (set in GitHub Settings → Environments):

- `APPLE_CERTIFICATE` — base64-encoded `.p12` of the Developer ID Application cert.
- `APPLE_CERTIFICATE_PASSWORD` — the .p12 unlock password.
- `APPLE_SIGNING_IDENTITY` — e.g. `Developer ID Application: Rossum AG (TEAMID)`.
- `APPLE_ID` — Apple ID with App Store Connect access.
- `APPLE_TEAM_ID` — 10-character team id.
- `APPLE_APP_SPECIFIC_PASSWORD` — app-specific password for notarytool.
- `TAURI_SIGNING_PRIVATE_KEY` — contents of the Ed25519 key file from Task 23 Step 1.
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — passphrase for that key (empty if generated without one).

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/desktop-release.yml`:

```yaml
name: Desktop release

on:
  push:
    tags:
      - "desktop-v*"

jobs:
  build:
    runs-on: macos-14
    environment: desktop-release
    timeout-minutes: 60

    steps:
      - uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: x86_64-apple-darwin,aarch64-apple-darwin

      - name: Install tauri-cli
        run: cargo install tauri-cli --version "^2.0.0" --locked

      - name: Install Node + TypeScript
        uses: actions/setup-node@v4
        with:
          node-version: "20"

      - name: Import Apple Developer ID certificate
        env:
          APPLE_CERTIFICATE: ${{ secrets.APPLE_CERTIFICATE }}
          APPLE_CERTIFICATE_PASSWORD: ${{ secrets.APPLE_CERTIFICATE_PASSWORD }}
        run: |
          echo "$APPLE_CERTIFICATE" | base64 --decode > $RUNNER_TEMP/cert.p12
          security create-keychain -p actions build.keychain
          security default-keychain -s build.keychain
          security unlock-keychain -p actions build.keychain
          security import $RUNNER_TEMP/cert.p12 -k build.keychain -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign -T /usr/bin/security
          security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k actions build.keychain
          security list-keychains -d user -s build.keychain $(security list-keychains -d user | tr -d \")

      - name: Build, sign, notarize, staple
        env:
          APPLE_SIGNING_IDENTITY: ${{ secrets.APPLE_SIGNING_IDENTITY }}
          APPLE_ID: ${{ secrets.APPLE_ID }}
          APPLE_TEAM_ID: ${{ secrets.APPLE_TEAM_ID }}
          APPLE_PASSWORD: ${{ secrets.APPLE_APP_SPECIFIC_PASSWORD }}
          TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
          TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
        run: |
          cd rossum-local
          cargo tauri build --target universal-apple-darwin

      - name: Generate latest.json for updater feed
        run: |
          VERSION=${GITHUB_REF_NAME#desktop-v}
          DMG=$(ls rossum-local/target/universal-apple-darwin/release/bundle/dmg/*.dmg)
          SIG=$(cat $DMG.sig)
          cat > rossum-local-latest.json <<EOF
          {
            "version": "$VERSION",
            "notes": "See https://github.com/mrtnzlml/rdc/releases/tag/$GITHUB_REF_NAME",
            "pub_date": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
            "platforms": {
              "darwin-aarch64": {
                "signature": "$SIG",
                "url": "https://github.com/mrtnzlml/rdc/releases/download/$GITHUB_REF_NAME/$(basename $DMG)"
              },
              "darwin-x86_64": {
                "signature": "$SIG",
                "url": "https://github.com/mrtnzlml/rdc/releases/download/$GITHUB_REF_NAME/$(basename $DMG)"
              }
            }
          }
          EOF

      - name: Upload to GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          files: |
            rossum-local/target/universal-apple-darwin/release/bundle/dmg/*.dmg
            rossum-local/target/universal-apple-darwin/release/bundle/dmg/*.dmg.sig
            rossum-local-latest.json
          fail_on_unmatched_files: true
          generate_release_notes: true
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

- [ ] **Step 2: Document secret setup in a comment**

Add a comment block at the top of the workflow listing the required secrets (already in the task brief above). Engineers configuring CI consult this.

- [ ] **Step 3: Smoke test the workflow file**

Validate YAML syntax:

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/desktop-release.yml'))"
```

Expected: no output (valid YAML).

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/desktop-release.yml
git commit -m "ci(desktop): GitHub Actions workflow to sign + notarize + DMG on desktop-v* tag"
```

---

### Task 25: README section + icon

**Files:**
- Modify: `README.md`
- Replace: `rossum-local/icons/icon.png` with a real 1024×1024 design

- [ ] **Step 1: Add a "Desktop app (macOS)" section to README.md**

Insert after the existing "Install" section in `README.md`:

```markdown
## Desktop app (macOS)

For non-developers: **Rossum Local** is a signed macOS app that pulls Rossum orgs locally so [Claude](https://claude.ai) can read them — no terminal, no git.

- Download: [latest release](https://github.com/mrtnzlml/rdc/releases?q=desktop&expanded=true)
- Source: `rossum-local/` in this repo.

The app is a thin GUI over the same `rdc` engine. Pull-only in v1; the folder it produces is identical to what `rdc init && rdc sync main` creates.
```

- [ ] **Step 2: Produce a real icon**

Design or commission a 1024×1024 PNG (amber accent on a clean ground, matching rdc's CLI palette `#ED8E47`). Save as `rossum-local/icons/icon.png`. Then regenerate the icns:

```bash
cd rossum-local
cargo tauri icon icons/icon.png
```

This produces `icons/icon.icns`, `icons/32x32.png`, `icons/128x128.png`, `icons/128x128@2x.png`, and `icons/Square*Logo.png` variants. The `.icns` is what `tauri.conf.json` references.

- [ ] **Step 3: Smoke test bundle**

```bash
cargo tauri build --target universal-apple-darwin
```

Expected: bundle succeeds locally (without signing — `signingIdentity: null`). The unsigned `.app` opens via right-click → Open the first time.

- [ ] **Step 4: Commit**

```bash
git add README.md rossum-local/icons/
git commit -m "docs(readme): add Desktop app section; icons: real 1024x1024 placeholder"
```

---

## Phase 12 — Verification + first release

### Task 26: Manual smoke checklist + version bump + first tag

**Files:**
- Modify: `rossum-local/Cargo.toml` (version bump)
- Modify: `Cargo.toml` (workspace member version, if relevant)

- [ ] **Step 1: Run the manual checklist**

A human (the engineer + at least one non-developer reviewer) walks through the spec's success criteria, end to end:

1. Install: download latest `.dmg`, drag to Applications, double-click. No Gatekeeper warning.
2. Empty state shows. Click "Add Connection".
3. Add a real Rossum Connection in token mode. Verify the folder appears under `~/Documents/Rossum/<slug>/` with `CLAUDE.md`, `rdc.toml`, `envs/main/_index.md`.
4. Click "Reveal in Finder". Finder opens the folder.
5. Click "Copy path". Paste elsewhere — it's a clean `/Users/...` path.
6. `cd` into the folder and run `claude` (Claude Code). Verify Claude sees `CLAUDE.md` and `_index.md`.
7. Drag the folder into a claude.ai Project. Verify the Project contains the files.
8. Click "Sync now". Progress bar appears; on completion, file count updates.
9. Edit credentials: switch from token to password mode. Re-sync. Works.
10. Remove Connection. Confirm. Folder appears in Trash.
11. Light and dark mode: toggle macOS appearance. The app re-themes.
12. Resize the window down to minimum. No clipped content.
13. Quit and relaunch. The registry persists; previously-added Connections are still listed.

For each item, mark ✓ or note the failure. Any failure blocks the release.

- [ ] **Step 2: Bump the version**

Edit `rossum-local/Cargo.toml`:

```toml
version = "0.1.0"
```

(already at 0.1.0; bump to 0.1.1 only if the smoke checklist surfaced a fix.)

Edit `rossum-local/tauri.conf.json`:

```json
"version": "0.1.0"
```

- [ ] **Step 3: Tag the release**

```bash
git tag desktop-v0.1.0
git push origin desktop-v0.1.0
```

Expected: the GitHub Actions workflow from Task 24 runs and produces a signed DMG.

- [ ] **Step 4: Verify the release**

On a clean Mac (or VM):

1. Download the DMG from the GitHub Release.
2. Verify Gatekeeper passes silently (notarization stapled).
3. Walk through the smoke checklist (Step 1) on this fresh install.

- [ ] **Step 5: No commit needed**

The tag is the artifact.

---

## Self-review checklist (for the plan author)

Before handing this plan to an executor, the author runs:

1. **Spec coverage:**
   - All 7 success criteria in the spec → covered by Tasks 14 (Add), 11 (Sync), 19 (Reveal/Copy), 7-8 (Keychain), 12 (queue), 20 (Remove).
   - Architecture diagram → Tasks 1, 2, 9, 13.
   - On-disk layout (rdc-native, env=main) → Task 9 (embed), Task 10 (rdc.toml), Task 11 (orchestrator).
   - Keychain (three entries) → Task 7.
   - Connection registry schema → Task 4.
   - UI views (empty, detail, Add, Edit, Remove, Settings, Diagnostics) → Tasks 16, 17, 18, 20, 21, 22.
   - Sync orchestration phases → Task 11, 12, 15 (events emitted, but only 3 phases vs spec's 5; **flag for executor:** consider emitting finer phases if frontend wants them).
   - Error-handling table → covered by `SyncError` taxonomy in Task 11 and command-level Err returns; explicit per-case banners are wired in Task 18.
   - Distribution (signed DMG, updater) → Tasks 23, 24, 25.
   - Testing (unit + Tauri + Playwright + manual) → Tasks 4-12 cover unit; manual checklist in Task 26. **Gap:** Playwright suite from spec §Testing is not in the plan. Defer to v0.2.0 unless the engineer wants to add it now (one extra task).

2. **Placeholder scan:** searched for TBD/TODO/FIXME — none in the plan body. The phrase "filled in by subsequent tasks" in Task 2 Step 4 is a deliberate scaffold note, not a placeholder.

3. **Type consistency:**
   - `ConnectionStatus::Error(String)` in Task 4 vs. `last_status_message` in `ConnectionSummary` (Task 13): the From impl maps them. Consistent.
   - `SyncProgress.phase` is `"started" | "done" | "error"` in Task 15; frontend listener (Task 18) handles the same three. Consistent.
   - `AuthKind::{Token, Password}` used everywhere consistently.
   - `TokenEntry { token, expires_at_unix }` used in Task 7, 8, 14, 20 with the same field names.

4. **Ambiguity check:**
   - "Folder picker" in Add Connection (spec) is implemented as a plain text field in Task 17 with no native picker. **Flag for executor:** add `tauri-plugin-dialog`'s folder-picker call in Task 17 Step 1 to match the spec's "Change…" button.
   - The Settings window in Task 21 is a sheet (modal), not a separate window as the spec says. The spec is permissive here; a sheet is simpler and acceptable. No change.

Fix the two flagged gaps inline if the executor wants strict spec parity, or accept them as v0.1.0 simplifications.

---

## Execution

Plan complete. Two execution options:

1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task, review between tasks, fast iteration. Use the superpowers:subagent-driven-development skill.
2. **Inline Execution** — execute tasks in this session via superpowers:executing-plans, batch with checkpoints.

Pick one when you're ready to start building.



