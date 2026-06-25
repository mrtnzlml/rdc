# Live Integration Test Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an opt-in, repeatable live-API integration suite that drives the real `rdc` binary/lib against a real Rossum test org and verifies both remote API state and the resulting local files/lockfile, covering round-trip sync and the migrate+sync deploy flow.

**Architecture:** A new `tests/live.rs` test binary with a `tests/live/support/` module of reusable primitives (config gating, run-id namespacing, a thin wrapper over `rdc::api::RossumClient`, a declarative seeder, a tempdir project fixture, local/remote asserters, and an RAII teardown). Edge-case resources live in a declarative `testdata/live/` static folder. Live scenarios are `#[ignore]` (so `cargo test` never runs them); the harness's pure logic is covered by fast hermetic unit tests that DO run by default.

**Tech Stack:** Rust 2021/2024 (match repo), `tokio` (async tests), `assert_cmd` (drive the binary), `tempfile` (sandbox), `serde_json`/`toml` (bodies + manifests), `pretty_assertions` (diffs), and the `rdc` library crate (`rdc::api::RossumClient`, `rdc::config`, `rdc::state`, `rdc::paths`). No new dependencies.

## Global Constraints

- **No new dependencies.** Use only crates already in `Cargo.toml` dev-deps: `wiremock`, `assert_cmd`, `predicates`, `pretty_assertions`, `proptest`, `tokio` (test-util), plus the `rdc` lib and std.
- **`cargo test` must stay green and fast.** Every live scenario is annotated `#[ignore = "live: needs RDC_LIVE_* env"]`. Only fast, hermetic, network-free unit tests run by default.
- **No sandbox coordinates in the repo.** Org id, host, and token come exclusively from env vars `RDC_LIVE_API_BASE`, `RDC_LIVE_ORG_ID`, `RDC_LIVE_TOKEN` (optionally `RDC_LIVE_USER`/`RDC_LIVE_PASS`). Tests skip-with-message when any required var is unset.
- **No customer identifiers anywhere** (code, fixtures, commit messages). Use neutral placeholders only: `acme`, `invoices`, `orders`, `main`, `secondary`, `validator`, `test`, `prod`.
- **No assumptions on uncertain output.** Where the exact on-disk form of an edge case is not already proven by the codebase (e.g. cross-workspace same-name dedup, exact composite `rdc://` ref form), use the capture-then-review workflow: run once live, capture the actual normalized local state into `expected/*.toml`, the maintainer reviews it for correctness, then it is committed as the golden expectation.
- **All objects are creatable/deletable via API given correct call ordering.** Teardown deletes children before parents in the order: engine_fields → engines → labels → rules → hooks → email_templates → inboxes → queues → schemas → workspaces. Deleting a queue removes its auto-created schema/templates; do not delete those directly.
- **Verified API facts (do not re-derive):** `RossumClient::new(api_base, token)` where `api_base` is the env value verbatim. `ProgressHandle = Option<Arc<rdc::log::Log>>`; pass `None` for silence. All `create_*` take `&serde_json::Value` and return a typed model with `pub id: u64` and `pub url: String`. `delete_*(id, progress)` exist per kind; generic `delete_path(path, progress)` and `patch_value(path, body, progress)` also exist. Organization is pull-only (never POST/PATCH/DELETE).

---

## File Structure

```
tests/
  live.rs                          # crate root: `mod support; mod scenarios;`
  live/
    support/
      mod.rs                       # pub use of each submodule
      config.rs                    # LiveConfig::from_env() -> Option<LiveConfig>
      run_id.rs                    # RunId: unique per-process, slug-safe prefix
      manifest.rs                  # Manifest/ObjectSpec parse + topo order
      refs.rs                      # @kind/key placeholder resolution
      client.rs                    # LiveClient: thin wrapper over RossumClient
      project.rs                   # ProjectFixture: tempdir + rdc.toml + secrets + run rdc
      seeder.rs                    # Seeder: load manifest -> POST in order -> SeedIndex
      teardown.rs                  # Teardown: RAII Drop, dependency-order deletes
      expected.rs                  # Expected: load/compare/capture golden state
      assert_local.rs              # local snapshot + lockfile readers/normalizers
      assert_remote.rs             # remote GET + structural assertions
    scenarios/
      mod.rs                       # `mod round_trip; mod collisions; ...`
      round_trip.rs                # live_round_trip_core
      collisions.rs                # live_collisions_identity
      cross_refs.rs                # live_cross_refs
      sidecars.rs                  # live_sidecars_redaction
      conflicts_deletes.rs         # live_conflicts_deletes
      deploy_flow.rs               # live_deploy_flow
      janitor.rs                   # live_janitor_sweep
testdata/live/
  manifest.toml
  bodies/<kind>/<key>.json
  bodies/hooks/<key>.py
  expected/<scenario>.toml
README.md                          # add "Live integration testing" section
```

**Module wiring note (Rust integration tests):** Files in `tests/` are separate test crates; subdirectories are NOT auto-compiled. `tests/live.rs` is the crate root and pulls in everything via `mod support;` and `mod scenarios;`. `#[test]`/`#[tokio::test]` functions are discovered in any module of the crate, so scenarios in submodules are found. `#[ignore]` affects only runtime, not compilation, so support code referenced solely by ignored tests is not dead code.

---

## Task 1: Scaffold the live test binary, config gating, and run-id

**Files:**
- Create: `tests/live.rs`
- Create: `tests/live/support/mod.rs`
- Create: `tests/live/support/config.rs`
- Create: `tests/live/support/run_id.rs`
- Create: `tests/live/scenarios/mod.rs`

**Interfaces:**
- Produces:
  - `pub struct LiveConfig { pub api_base: String, pub org_id: u64, pub token: String }`
  - `pub fn LiveConfig::from_env() -> Option<LiveConfig>` (None if any required var unset)
  - `pub fn LiveConfig::skip_reason() -> String` (human message listing missing vars)
  - `pub struct RunId(String)` with `pub fn new() -> RunId`, `pub fn as_str(&self) -> &str`, `pub fn prefix(&self, name: &str) -> String` (returns `rdc-it-<id>-<name>`), and `pub fn marker() -> &'static str` (`"rdc-it-"`)

- [ ] **Step 1: Create the crate root and module tree**

`tests/live.rs`:
```rust
//! Live integration tests against a real Rossum test org.
//!
//! These are `#[ignore]` by default — `cargo test` never runs them. Opt in
//! with credentials in the environment:
//!
//! ```sh
//! export RDC_LIVE_API_BASE="https://<host>/v1"
//! export RDC_LIVE_ORG_ID="<org id>"
//! export RDC_LIVE_TOKEN="<token>"
//! cargo test --test live -- --ignored --test-threads=1
//! ```
//!
//! The harness's pure logic (config, run-id, manifest, ref resolution,
//! project fixture, expectations) is covered by fast hermetic unit tests in
//! the `support` modules, which DO run under a plain `cargo test`.

mod support;
mod scenarios;
```

`tests/live/support/mod.rs`:
```rust
pub mod config;
pub mod run_id;
```

`tests/live/scenarios/mod.rs`:
```rust
// Scenario modules are added by later tasks.
```

- [ ] **Step 2: Write the failing test for config gating**

`tests/live/support/config.rs`:
```rust
use std::sync::{Mutex, OnceLock};

/// Serializes tests that mutate process-global env vars.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Resolved live-test configuration. All fields come from the environment;
/// the repo never hardcodes a host, org id, or token.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    pub api_base: String,
    pub org_id: u64,
    pub token: String,
}

impl LiveConfig {
    /// Returns `Some` only when every required var is present and parseable.
    /// Token may come from `RDC_LIVE_TOKEN`.
    pub fn from_env() -> Option<LiveConfig> {
        let api_base = std::env::var("RDC_LIVE_API_BASE").ok()?;
        let org_id = std::env::var("RDC_LIVE_ORG_ID").ok()?.parse::<u64>().ok()?;
        let token = std::env::var("RDC_LIVE_TOKEN").ok()?;
        if api_base.is_empty() || token.is_empty() {
            return None;
        }
        Some(LiveConfig { api_base, org_id, token })
    }

    /// Human-readable reason printed when a live scenario skips.
    pub fn skip_reason() -> String {
        "SKIP live test: set RDC_LIVE_API_BASE, RDC_LIVE_ORG_ID, RDC_LIVE_TOKEN \
         to run (see tests/live.rs)"
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_is_none_when_vars_absent() {
        let _g = env_lock();
        // SAFETY: serialized by env_lock; restored below.
        unsafe {
            std::env::remove_var("RDC_LIVE_API_BASE");
            std::env::remove_var("RDC_LIVE_ORG_ID");
            std::env::remove_var("RDC_LIVE_TOKEN");
        }
        assert!(LiveConfig::from_env().is_none());
    }

    #[test]
    fn from_env_parses_when_all_present() {
        let _g = env_lock();
        unsafe {
            std::env::set_var("RDC_LIVE_API_BASE", "https://example.rossum.app/api/v1");
            std::env::set_var("RDC_LIVE_ORG_ID", "12345");
            std::env::set_var("RDC_LIVE_TOKEN", "tok");
        }
        let cfg = LiveConfig::from_env().expect("config present");
        assert_eq!(cfg.org_id, 12345);
        assert_eq!(cfg.api_base, "https://example.rossum.app/api/v1");
        assert_eq!(cfg.token, "tok");
        unsafe {
            std::env::remove_var("RDC_LIVE_API_BASE");
            std::env::remove_var("RDC_LIVE_ORG_ID");
            std::env::remove_var("RDC_LIVE_TOKEN");
        }
    }
}
```

- [ ] **Step 3: Run the config tests to verify they fail (not yet wired) then pass**

Run: `cargo test --test live config:: -- --nocapture`
Expected: compiles and PASSES (the module already contains the implementation). If it does not compile, fix module paths in `tests/live.rs`.

- [ ] **Step 4: Write the failing test for `RunId`**

`tests/live/support/run_id.rs`:
```rust
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-process unique, slug-safe namespace token. Every seeded object name is
/// prefixed with `rdc-it-<id>-` so concurrent or crashed runs never collide
/// and a janitor can identify leftovers.
#[derive(Debug, Clone)]
pub struct RunId(String);

impl RunId {
    pub fn new() -> RunId {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id() as u128;
        // base36 of (nanos XOR-mixed with pid), lowercase alnum only.
        let mixed = nanos.wrapping_mul(1_000_003).wrapping_add(pid);
        RunId(format!("{}", to_base36(mixed)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `rdc-it-<id>-<name>` — used as the *display name* sent to the API.
    pub fn prefix(&self, name: &str) -> String {
        format!("{}{}-{}", Self::marker(), self.0, name)
    }

    /// Stable substring shared by every object this harness creates.
    pub fn marker() -> &'static str {
        "rdc-it-"
    }
}

fn to_base36(mut n: u128) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_slug_safe_and_marked() {
        let id = RunId::new();
        let p = id.prefix("Invoices Alpha");
        assert!(p.starts_with(RunId::marker()));
        assert!(p.contains("-Invoices Alpha"));
        // the id segment is lowercase alnum
        assert!(id.as_str().chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert!(!id.as_str().is_empty());
    }
}
```

- [ ] **Step 5: Wire run_id into the support module and run**

Edit `tests/live/support/mod.rs` — already contains `pub mod run_id;` from Step 1. Run:
Run: `cargo test --test live -- --nocapture`
Expected: PASS (config + run_id unit tests). No scenarios exist yet.

- [ ] **Step 6: Commit**

```bash
git add tests/live.rs tests/live/support/mod.rs tests/live/support/config.rs \
        tests/live/support/run_id.rs tests/live/scenarios/mod.rs
git commit -m "test(live): scaffold live test binary with config gating and run-id"
```

---

## Task 2: Manifest model + topological order + placeholder resolution

**Files:**
- Create: `tests/live/support/manifest.rs`
- Create: `tests/live/support/refs.rs`
- Modify: `tests/live/support/mod.rs` (add `pub mod manifest; pub mod refs;`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct ObjectSpec { pub key: String, pub kind: String, pub body: String, pub deps: Vec<String>, pub tags: Vec<String> }`
  - `pub struct Manifest { pub objects: Vec<ObjectSpec> }`
  - `pub fn Manifest::parse(toml_src: &str) -> anyhow::Result<Manifest>`
  - `pub fn Manifest::topo_order(&self) -> anyhow::Result<Vec<&ObjectSpec>>` (dependency-respecting; errors on cycle/unknown dep)
  - `pub fn resolve_placeholders(body: &mut serde_json::Value, lookup: &dyn Fn(&str, &str) -> Option<String>)` — replaces every string of the form `@<kind>/<key>` with `lookup(kind, key)`; panics via `Result` if unresolved. Signature: `pub fn resolve_placeholders(body: &mut serde_json::Value, resolved: &std::collections::BTreeMap<(String, String), String>) -> anyhow::Result<()>`

- [ ] **Step 1: Write the failing test for manifest parse + topo order**

`tests/live/support/manifest.rs`:
```rust
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Deserialize)]
pub struct ObjectSpec {
    pub key: String,
    pub kind: String,
    pub body: String,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    #[serde(rename = "object", default)]
    pub objects: Vec<ObjectSpec>,
}

impl Manifest {
    pub fn parse(toml_src: &str) -> Result<Manifest> {
        let m: Manifest = toml::from_str(toml_src).map_err(|e| anyhow!("parsing manifest: {e}"))?;
        // keys must be unique
        let mut seen = BTreeSet::new();
        for o in &m.objects {
            if !seen.insert(o.key.clone()) {
                bail!("duplicate manifest key: {}", o.key);
            }
        }
        Ok(m)
    }

    /// Kahn topological sort over `deps`. Errors on unknown dep or cycle.
    pub fn topo_order(&self) -> Result<Vec<&ObjectSpec>> {
        let by_key: BTreeMap<&str, &ObjectSpec> =
            self.objects.iter().map(|o| (o.key.as_str(), o)).collect();
        for o in &self.objects {
            for d in &o.deps {
                if !by_key.contains_key(d.as_str()) {
                    bail!("object {} depends on unknown key {}", o.key, d);
                }
            }
        }
        let mut indeg: BTreeMap<&str, usize> =
            self.objects.iter().map(|o| (o.key.as_str(), o.deps.len())).collect();
        let mut ready: Vec<&str> =
            indeg.iter().filter(|(_, &d)| d == 0).map(|(&k, _)| k).collect();
        ready.sort();
        let mut out = Vec::new();
        while let Some(k) = ready.pop() {
            out.push(by_key[k]);
            for o in &self.objects {
                if o.deps.iter().any(|d| d == k) {
                    let e = indeg.get_mut(o.key.as_str()).unwrap();
                    *e -= 1;
                    if *e == 0 {
                        ready.push(o.key.as_str());
                        ready.sort();
                    }
                }
            }
        }
        if out.len() != self.objects.len() {
            bail!("dependency cycle in manifest");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[object]]
key = "ws"
kind = "workspace"
body = "bodies/workspaces/ws.json"

[[object]]
key = "schema"
kind = "schema"
body = "bodies/schemas/schema.json"

[[object]]
key = "queue"
kind = "queue"
body = "bodies/queues/queue.json"
deps = ["ws", "schema"]
tags = ["core"]
"#;

    #[test]
    fn parses_and_orders_deps_first() {
        let m = Manifest::parse(SAMPLE).unwrap();
        assert_eq!(m.objects.len(), 3);
        let order: Vec<&str> = m.topo_order().unwrap().iter().map(|o| o.key.as_str()).collect();
        let qpos = order.iter().position(|k| *k == "queue").unwrap();
        let wpos = order.iter().position(|k| *k == "ws").unwrap();
        let spos = order.iter().position(|k| *k == "schema").unwrap();
        assert!(wpos < qpos && spos < qpos, "deps must precede dependent: {order:?}");
    }

    #[test]
    fn errors_on_unknown_dep() {
        let bad = "[[object]]\nkey=\"a\"\nkind=\"queue\"\nbody=\"b\"\ndeps=[\"missing\"]\n";
        assert!(Manifest::parse(bad).unwrap().topo_order().is_err());
    }
}
```

- [ ] **Step 2: Run topo tests**

Run: `cargo test --test live manifest:: -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Write the failing test for placeholder resolution**

`tests/live/support/refs.rs`:
```rust
use anyhow::{bail, Result};
use std::collections::BTreeMap;

/// Replace every string of the form `@<kind>/<key>` anywhere in `body` with
/// the resolved URL from `resolved[(kind, key)]`. Errors if a placeholder has
/// no resolution (a dependency that was not created/declared first).
pub fn resolve_placeholders(
    body: &mut serde_json::Value,
    resolved: &BTreeMap<(String, String), String>,
) -> Result<()> {
    match body {
        serde_json::Value::String(s) => {
            if let Some(rest) = s.strip_prefix('@') {
                let (kind, key) = rest
                    .split_once('/')
                    .ok_or_else(|| anyhow::anyhow!("bad placeholder '{s}': expected @kind/key"))?;
                match resolved.get(&(kind.to_string(), key.to_string())) {
                    Some(url) => *s = url.clone(),
                    None => bail!("unresolved placeholder '{s}'"),
                }
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            for it in items {
                resolve_placeholders(it, resolved)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (_k, v) in map.iter_mut() {
                resolve_placeholders(v, resolved)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_nested_placeholders() {
        let mut body = json!({
            "name": "Q",
            "workspace": "@workspace/ws",
            "hooks": ["@hook/v1", "literal"],
        });
        let mut r = BTreeMap::new();
        r.insert(("workspace".into(), "ws".into()), "https://h/v1/workspaces/1".into());
        r.insert(("hook".into(), "v1".into()), "https://h/v1/hooks/9".into());
        resolve_placeholders(&mut body, &r).unwrap();
        assert_eq!(body["workspace"], "https://h/v1/workspaces/1");
        assert_eq!(body["hooks"][0], "https://h/v1/hooks/9");
        assert_eq!(body["hooks"][1], "literal");
    }

    #[test]
    fn errors_on_unresolved() {
        let mut body = json!({ "workspace": "@workspace/missing" });
        assert!(resolve_placeholders(&mut body, &BTreeMap::new()).is_err());
    }
}
```

- [ ] **Step 4: Add modules and run**

Edit `tests/live/support/mod.rs`:
```rust
pub mod config;
pub mod run_id;
pub mod manifest;
pub mod refs;
```
Run: `cargo test --test live -- --nocapture`
Expected: PASS (config, run_id, manifest, refs).

- [ ] **Step 5: Commit**

```bash
git add tests/live/support/manifest.rs tests/live/support/refs.rs tests/live/support/mod.rs
git commit -m "test(live): manifest parse/topo-order and @kind/key placeholder resolution"
```

---

## Task 3: Project fixture (tempdir + rdc.toml + secrets + run rdc)

**Files:**
- Create: `tests/live/support/project.rs`
- Modify: `tests/live/support/mod.rs` (add `pub mod project;`)

**Interfaces:**
- Consumes: `LiveConfig` (Task 1).
- Produces:
  - `pub struct ProjectFixture { dir: tempfile::TempDir }`
  - `pub fn ProjectFixture::init(cfg: &LiveConfig, envs: &[&str]) -> anyhow::Result<ProjectFixture>` — runs `rdc init --env <name>=<api_base>:<org_id>` for each env, then writes `secrets/<env>.secrets.json`.
  - `pub fn ProjectFixture::path(&self) -> &std::path::Path`
  - `pub fn ProjectFixture::run_rdc(&self, args: &[&str]) -> std::process::Output` — `assert_cmd` spawn with `.current_dir(path)`.
  - `pub fn ProjectFixture::read_json(&self, rel: &str) -> serde_json::Value`
  - `pub fn ProjectFixture::read_to_string(&self, rel: &str) -> Option<String>`
  - `pub fn ProjectFixture::exists(&self, rel: &str) -> bool`

- [ ] **Step 1: Write the failing test for fixture bootstrap (no network)**

`tests/live/support/project.rs`:
```rust
use crate::support::config::LiveConfig;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Output;
use tempfile::TempDir;

pub struct ProjectFixture {
    dir: TempDir,
}

impl ProjectFixture {
    /// Bootstrap a tempdir project with one or more envs all pointing at the
    /// live org, plus a secrets file per env containing the live token.
    pub fn init(cfg: &LiveConfig, envs: &[&str]) -> Result<ProjectFixture> {
        let dir = TempDir::new().context("creating tempdir")?;
        let mut args: Vec<String> = vec!["init".into()];
        for env in envs {
            args.push("--env".into());
            args.push(format!("{}={}:{}", env, cfg.api_base, cfg.org_id));
        }
        let out = assert_cmd::Command::cargo_bin("rdc")
            .context("locating rdc binary")?
            .current_dir(dir.path())
            .args(&args)
            .output()
            .context("running rdc init")?;
        anyhow::ensure!(
            out.status.success(),
            "rdc init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        for env in envs {
            let secrets = serde_json::json!({ "api_token": cfg.token });
            std::fs::write(
                dir.path().join(format!("secrets/{env}.secrets.json")),
                serde_json::to_vec_pretty(&secrets)?,
            )
            .with_context(|| format!("writing secrets for {env}"))?;
        }
        Ok(ProjectFixture { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn run_rdc(&self, args: &[&str]) -> Output {
        assert_cmd::Command::cargo_bin("rdc")
            .unwrap()
            .current_dir(self.dir.path())
            .args(args)
            .output()
            .expect("spawning rdc")
    }

    pub fn read_json(&self, rel: &str) -> serde_json::Value {
        let raw = std::fs::read_to_string(self.dir.path().join(rel))
            .unwrap_or_else(|e| panic!("reading {rel}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {rel}: {e}"))
    }

    pub fn read_to_string(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join(rel)).ok()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.dir.path().join(rel).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hermetic: uses a fake config and asserts the files `rdc init` writes.
    // Requires the `rdc` binary to be built (it is, as the test target's bin
    // dependency), but makes NO network calls.
    #[test]
    fn init_writes_rdc_toml_and_secrets() {
        let cfg = LiveConfig {
            api_base: "https://example.rossum.app/api/v1".into(),
            org_id: 999,
            token: "tok".into(),
        };
        let p = ProjectFixture::init(&cfg, &["test", "prod"]).unwrap();
        assert!(p.exists("rdc.toml"));
        let toml = p.read_to_string("rdc.toml").unwrap();
        assert!(toml.contains("[envs.test]"));
        assert!(toml.contains("[envs.prod]"));
        assert!(toml.contains("api_base = \"https://example.rossum.app/api/v1\""));
        let sec = p.read_json("secrets/test.secrets.json");
        assert_eq!(sec["api_token"], "tok");
    }
}
```

- [ ] **Step 2: Add module + run**

Edit `tests/live/support/mod.rs` → add `pub mod project;`.
Run: `cargo test --test live project:: -- --nocapture`
Expected: PASS (this builds the `rdc` binary and runs `init` locally — no network).

- [ ] **Step 3: Commit**

```bash
git add tests/live/support/project.rs tests/live/support/mod.rs
git commit -m "test(live): ProjectFixture bootstraps tempdir rdc project + secrets"
```

---

## Task 4: Live API client wrapper

**Files:**
- Create: `tests/live/support/client.rs`
- Modify: `tests/live/support/mod.rs` (add `pub mod client;`)

**Interfaces:**
- Consumes: `LiveConfig` (Task 1); `rdc::api::RossumClient`.
- Produces:
  - `pub struct LiveClient { inner: rdc::api::RossumClient, pub org_url: String }`
  - `pub fn LiveClient::connect(cfg: &LiveConfig) -> anyhow::Result<LiveClient>` (builds client; computes `org_url = format!("{}/organizations/{}", cfg.api_base, cfg.org_id)`)
  - `pub async fn LiveClient::create(&self, kind: &str, body: &serde_json::Value) -> anyhow::Result<(u64, String)>` — dispatch to the right `create_*`, return `(id, url)`.
  - `pub async fn LiveClient::delete(&self, kind: &str, id: u64) -> anyhow::Result<()>` — dispatch to the right `delete_*`.
  - `pub async fn LiveClient::list_ids_by_name_prefix(&self, kind: &str, prefix: &str) -> anyhow::Result<Vec<(u64, String)>>` — list a kind, return `(id, name)` for objects whose name starts with `prefix`.
  - `pub async fn LiveClient::get_value(&self, kind: &str, id: u64) -> anyhow::Result<serde_json::Value>` — GET one object as raw JSON via `delete_path`'s sibling pattern using `patch_value` is wrong; use a typed get and serialize. (Implementation below uses typed getters + `serde_json::to_value`.)

- [ ] **Step 1: Implement the client wrapper**

`tests/live/support/client.rs`:
```rust
use crate::support::config::LiveConfig;
use anyhow::{anyhow, Result};
use rdc::api::RossumClient;

pub struct LiveClient {
    inner: RossumClient,
    pub org_url: String,
}

impl LiveClient {
    pub fn connect(cfg: &LiveConfig) -> Result<LiveClient> {
        let inner = RossumClient::new(cfg.api_base.clone(), cfg.token.clone())?;
        let org_url = format!("{}/organizations/{}", cfg.api_base.trim_end_matches('/'), cfg.org_id);
        Ok(LiveClient { inner, org_url })
    }

    /// Create an object of `kind` from a fully-resolved body. Returns the
    /// server-assigned (id, url). `None` progress = silent.
    pub async fn create(&self, kind: &str, body: &serde_json::Value) -> Result<(u64, String)> {
        let (id, url) = match kind {
            "workspace" => {
                let w = self.inner.create_workspace(body, None).await?;
                (w.id, w.url)
            }
            "queue" => {
                let q = self.inner.create_queue(body, None).await?;
                (q.id, q.url)
            }
            "schema" => {
                let s = self.inner.create_schema(body, None).await?;
                (s.id, s.url)
            }
            "hook" => {
                let h = self.inner.create_hook(body, None).await?;
                (h.id, h.url)
            }
            "inbox" => {
                let i = self.inner.create_inbox(body, None).await?;
                (i.id, i.url)
            }
            "label" => {
                let l = self.inner.create_label(body, None).await?;
                (l.id, l.url)
            }
            "rule" => {
                let r = self.inner.create_rule(body, None).await?;
                (r.id, r.url)
            }
            "email_template" => {
                let t = self.inner.create_email_template(body, None).await?;
                (t.id, t.url)
            }
            other => return Err(anyhow!("create: unsupported kind '{other}'")),
        };
        Ok((id, url))
    }

    pub async fn delete(&self, kind: &str, id: u64) -> Result<()> {
        match kind {
            "workspace" => self.inner.delete_workspace(id, None).await,
            "queue" => self.inner.delete_queue(id, None).await,
            "schema" => self.inner.delete_schema(id, None).await,
            "hook" => self.inner.delete_hook(id, None).await,
            "inbox" => self.inner.delete_inbox(id, None).await,
            "label" => self.inner.delete_label(id, None).await,
            "rule" => self.inner.delete_rule(id, None).await,
            "email_template" => self.inner.delete_email_template(id, None).await,
            other => Err(anyhow!("delete: unsupported kind '{other}'")),
        }
    }

    /// List a kind and return (id, name) for objects whose `name` starts with
    /// `prefix`. Used by teardown and the janitor.
    pub async fn list_ids_by_name_prefix(
        &self,
        kind: &str,
        prefix: &str,
    ) -> Result<Vec<(u64, String)>> {
        let values: Vec<serde_json::Value> = match kind {
            "workspace" => to_values(self.inner.list_workspaces(None).await?)?,
            "queue" => to_values(self.inner.list_queues(None).await?)?,
            "hook" => to_values(self.inner.list_hooks(None).await?)?,
            "label" => to_values(self.inner.list_labels(None).await?)?,
            "rule" => to_values(self.inner.list_rules(None).await?)?,
            "inbox" => to_values(self.inner.list_inboxes(None).await?)?,
            "email_template" => to_values(self.inner.list_email_templates(None).await?)?,
            other => return Err(anyhow!("list: unsupported kind '{other}'")),
        };
        let mut out = Vec::new();
        for v in values {
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.starts_with(prefix) {
                if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                    out.push((id, name.to_string()));
                }
            }
        }
        Ok(out)
    }

    /// Fetch one object as raw JSON (typed getter -> Value).
    pub async fn get_value(&self, kind: &str, id: u64) -> Result<serde_json::Value> {
        let v = match kind {
            "workspace" => serde_json::to_value(self.inner.get_workspace(id, None).await?)?,
            "hook" => serde_json::to_value(self.inner.get_hook(id, None).await?)?,
            "schema" => serde_json::to_value(self.inner.get_schema(id, None).await?)?,
            "inbox" => serde_json::to_value(self.inner.get_inbox(id, None).await?)?,
            other => return Err(anyhow!("get_value: unsupported kind '{other}'")),
        };
        Ok(v)
    }
}

fn to_values<T: serde::Serialize>(items: Vec<T>) -> Result<Vec<serde_json::Value>> {
    items.into_iter().map(|i| Ok(serde_json::to_value(i)?)).collect()
}
```

- [ ] **Step 2: Add module + verify it compiles**

Edit `tests/live/support/mod.rs` → add `pub mod client;`.
Run: `cargo test --test live --no-run`
Expected: compiles cleanly. (No unit test here; the client is exercised by live scenarios. If `list_queues`/`get_*`/`create_*` signatures differ, fix the call sites to match `src/api/mod.rs`.)

- [ ] **Step 3: Commit**

```bash
git add tests/live/support/client.rs tests/live/support/mod.rs
git commit -m "test(live): LiveClient wrapper over RossumClient (create/delete/list/get)"
```

---

## Task 5: Seeder + RAII Teardown

**Files:**
- Create: `tests/live/support/seeder.rs`
- Create: `tests/live/support/teardown.rs`
- Modify: `tests/live/support/client.rs` (add `schema_ids_for_queue_prefix` — schemas have no list endpoint)
- Modify: `tests/live/support/mod.rs` (add `pub mod seeder; pub mod teardown;`)

**Interfaces:**
- Consumes: `Manifest`/`ObjectSpec` (Task 2), `resolve_placeholders` (Task 2), `RunId` (Task 1), `LiveClient` (Task 4).
- Adds to `LiveClient`: `pub async fn schema_ids_for_queue_prefix(&self, prefix: &str) -> anyhow::Result<Vec<u64>>`.
- Produces:
  - `pub struct SeedIndex { by_key: BTreeMap<String, SeedEntry> }` with `pub fn url(&self, kind: &str, key: &str) -> Option<&str>`, `pub fn id(&self, key: &str) -> Option<u64>`, `pub fn entries(&self) -> impl Iterator<Item=&SeedEntry>`.
  - `pub struct SeedEntry { pub key: String, pub kind: String, pub id: u64, pub url: String, pub name: String }`
  - `pub async fn seed(client: &LiveClient, run_id: &RunId, dir: &Path, manifest: &Manifest) -> anyhow::Result<SeedIndex>` — topo-order, prefix names, inline sidecars, resolve placeholders, POST, record.
  - `pub struct Teardown<'a> { client: &'a LiveClient, run_id: RunId }` with `pub fn new(client: &'a LiveClient, run_id: RunId) -> Self`. `Drop` runs `block_on(teardown_by_marker(...))`.
  - `pub async fn teardown_by_prefix(client: &LiveClient, prefix: &str) -> anyhow::Result<()>` — delete all `prefix`-named objects in dependency order.

- [ ] **Step 1: Implement the seeder**

`tests/live/support/seeder.rs`:
```rust
use crate::support::client::LiveClient;
use crate::support::manifest::Manifest;
use crate::support::refs::resolve_placeholders;
use crate::support::run_id::RunId;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct SeedEntry {
    pub key: String,
    pub kind: String,
    pub id: u64,
    pub url: String,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct SeedIndex {
    by_key: BTreeMap<String, SeedEntry>,
}

impl SeedIndex {
    pub fn url(&self, _kind: &str, key: &str) -> Option<&str> {
        self.by_key.get(key).map(|e| e.url.as_str())
    }
    pub fn id(&self, key: &str) -> Option<u64> {
        self.by_key.get(key).map(|e| e.id)
    }
    pub fn entries(&self) -> impl Iterator<Item = &SeedEntry> {
        self.by_key.values()
    }
}

/// Create every object in the manifest, in dependency order, with names
/// prefixed by the run id. Cross-refs (`@kind/key`) resolve to the URL of the
/// already-created dependency. Hook code sidecars (`*.py` named by the body's
/// `config.code_file`) are inlined into `config.code` before POST.
pub async fn seed(
    client: &LiveClient,
    run_id: &RunId,
    dir: &Path,
    manifest: &Manifest,
) -> Result<SeedIndex> {
    let mut index = SeedIndex::default();
    // (kind, key) -> url, for placeholder resolution. Also map the special
    // "organization"/"self" to the org url so bodies can reference it.
    let mut resolved: BTreeMap<(String, String), String> = BTreeMap::new();
    resolved.insert(("organization".into(), "self".into()), client.org_url.clone());

    for spec in manifest.topo_order()? {
        let body_path = dir.join(&spec.body);
        let raw = std::fs::read_to_string(&body_path)
            .with_context(|| format!("reading body {}", body_path.display()))?;
        let mut body: serde_json::Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", spec.body))?;

        // Prefix the display name.
        if let Some(name) = body.get("name").and_then(|n| n.as_str()) {
            body["name"] = serde_json::Value::String(run_id.prefix(name));
        }

        // Inline a hook code sidecar if `config.code_file` is present.
        if let Some(code_file) = body
            .get("config")
            .and_then(|c| c.get("code_file"))
            .and_then(|f| f.as_str())
            .map(|s| s.to_string())
        {
            let code = std::fs::read_to_string(dir.join(&code_file))
                .with_context(|| format!("reading code sidecar {code_file}"))?;
            body["config"]["code"] = serde_json::Value::String(code);
            body["config"]
                .as_object_mut()
                .unwrap()
                .remove("code_file");
        }

        resolve_placeholders(&mut body, &resolved)?;

        let (id, url) = client
            .create(&spec.kind, &body)
            .await
            .with_context(|| format!("creating {} ({})", spec.key, spec.kind))?;

        let name = body.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
        resolved.insert((spec.kind.clone(), spec.key.clone()), url.clone());
        index.by_key.insert(
            spec.key.clone(),
            SeedEntry { key: spec.key.clone(), kind: spec.kind.clone(), id, url, name },
        );
    }
    Ok(index)
}
```

- [ ] **Step 2: Implement teardown**

First add this helper to `tests/live/support/client.rs` (`impl LiveClient`) — teardown needs it because schemas have no list endpoint:
```rust
/// Schemas have no list endpoint. Collect the schema ids referenced by queues
/// whose name starts with `prefix`, parsed from each queue's `schema` URL.
/// Call this BEFORE deleting the queues.
pub async fn schema_ids_for_queue_prefix(&self, prefix: &str) -> anyhow::Result<Vec<u64>> {
    let queues = self.inner.list_queues(None).await?;
    let mut out = Vec::new();
    for q in queues {
        if q.name.starts_with(prefix) {
            if let Some(url) = q.schema.as_deref() {
                if let Some(id) = url.trim_end_matches('/').rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) {
                    out.push(id);
                }
            }
        }
    }
    Ok(out)
}
```
> `Queue.schema` is `Option<String>` holding the remote schema URL; `list_queues` returns remote objects with real URLs (not `rdc://`), so the last-path-segment parse yields the schema id. `self.inner` is already in scope. Drop any now-unneeded `#[allow(dead_code)]` if this method's addition makes the client reachable; keep the build warning-free either way.

`tests/live/support/teardown.rs`:
```rust
use crate::support::client::LiveClient;
use crate::support::run_id::RunId;
use anyhow::Result;

/// Delete every object whose name starts with `prefix`, in dependency order:
/// children before parents. Schemas have NO list endpoint and rdc's delete
/// order is `queues -> schemas`, so schema ids are derived from the `schema`
/// URL of the prefix-matched queues (captured BEFORE the queues are deleted)
/// and deleted right after the queues. Tolerant: a not-found / already-deleting
/// object is logged, not fatal.
pub async fn teardown_by_prefix(client: &LiveClient, prefix: &str) -> Result<()> {
    // Capture schema ids BEFORE deleting queues (schemas can't be listed).
    let schema_ids = client.schema_ids_for_queue_prefix(prefix).await.unwrap_or_default();

    // Listable child kinds, in order, down to queues.
    for kind in ["email_template", "rule", "hook", "inbox", "queue"] {
        let found = match client.list_ids_by_name_prefix(kind, prefix).await {
            Ok(v) => v,
            Err(_) => continue, // kind not listable in isolation
        };
        for (id, name) in found {
            if let Err(e) = client.delete(kind, id).await {
                eprintln!("teardown: delete {kind} {id} ({name}) failed (continuing): {e:#}");
            }
        }
    }

    // Schemas: delete by derived id, now that their queues are gone (avoids 409).
    for id in schema_ids {
        if let Err(e) = client.delete("schema", id).await {
            eprintln!("teardown: delete schema {id} failed (continuing): {e:#}");
        }
    }

    // Parents last.
    for kind in ["workspace", "label"] {
        let found = match client.list_ids_by_name_prefix(kind, prefix).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        for (id, name) in found {
            if let Err(e) = client.delete(kind, id).await {
                eprintln!("teardown: delete {kind} {id} ({name}) failed (continuing): {e:#}");
            }
        }
    }
    Ok(())
}

/// RAII guard: on drop, deletes everything the run created. Holds its own
/// Tokio runtime handle so it can clean up even from a panicking test.
pub struct Teardown {
    client: LiveClient,
    run_id: RunId,
}

impl Teardown {
    pub fn new(client: LiveClient, run_id: RunId) -> Teardown {
        Teardown { client, run_id }
    }
    pub fn client(&self) -> &LiveClient {
        &self.client
    }
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        let prefix = format!("{}{}", RunId::marker(), self.run_id.as_str());
        // Build a short-lived runtime to run async deletes from Drop.
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("teardown: could not build runtime: {e}");
                return;
            }
        };
        if let Err(e) = rt.block_on(teardown_by_prefix(&self.client, &prefix)) {
            eprintln!("teardown: {e:#}");
        }
    }
}
```

> **Note for the implementer:** `Drop` spins up a `current_thread` runtime to run the async deletes. The live scenario itself must therefore run on a *multi-thread* runtime (use `#[tokio::test(flavor = "multi_thread")]`) so that creating a nested current-thread runtime inside `Drop` does not panic with "Cannot start a runtime from within a runtime". This is verified in Task 8.

- [ ] **Step 3: Add modules + compile**

Edit `tests/live/support/mod.rs` → add `pub mod seeder; pub mod teardown;`.
Run: `cargo test --test live --no-run`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add tests/live/support/seeder.rs tests/live/support/teardown.rs \
        tests/live/support/client.rs tests/live/support/mod.rs
git commit -m "test(live): Seeder (topo create) and RAII Teardown (dependency-order delete)"
```

---

## Task 6: Local + remote asserters and the Expected (capture/compare) helper

**Files:**
- Create: `tests/live/support/assert_local.rs`
- Create: `tests/live/support/assert_remote.rs`
- Create: `tests/live/support/expected.rs`
- Modify: `tests/live/support/mod.rs`

**Interfaces:**
- Consumes: `rdc::state::lockfile::Lockfile` (verified shape), `ProjectFixture` (Task 3), `LiveClient` (Task 4).
- Produces:
  - `pub fn load_lockfile(project: &Path, env: &str) -> anyhow::Result<rdc::state::lockfile::Lockfile>`
  - `pub fn lockfile_keys(lf: &Lockfile, kind: &str) -> Vec<String>` (sorted slugs for a kind)
  - `pub fn strip_volatile(v: &mut serde_json::Value)` — recursively remove `id`, `url`, `modified_at`, `created_at`, `created_by`, `modified_by`, and inbox `email`.
  - `pub fn field(v: &serde_json::Value, path: &str) -> Option<&serde_json::Value>` (dotted path getter)
  - `pub struct Expected { ... }` with `pub fn load_or_capture(path: &Path, actual: &CapturedState, capture: bool) -> anyhow::Result<()>` — when `RDC_LIVE_CAPTURE=1`, writes `actual` to `path` (golden) and passes; otherwise compares.
  - `pub struct CapturedState { pub lockfile_keys: BTreeMap<String, Vec<String>>, pub refs: BTreeMap<String, String> }` (serde, TOML)

- [ ] **Step 1: Implement local asserters**

`tests/live/support/assert_local.rs`:
```rust
use anyhow::{Context, Result};
use rdc::state::lockfile::Lockfile;
use std::path::Path;

pub fn load_lockfile(project: &Path, env: &str) -> Result<Lockfile> {
    let p = project.join(format!(".rdc/state/{env}.lock.json"));
    let raw = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

/// Sorted slugs recorded under `kind` in the lockfile (e.g. "queues").
pub fn lockfile_keys(lf: &Lockfile, kind: &str) -> Vec<String> {
    let mut v: Vec<String> = lf
        .objects
        .get(kind)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    v.sort();
    v
}

const VOLATILE: &[&str] = &[
    "id", "url", "modified_at", "created_at", "created_by", "modified_by", "email",
];

/// Recursively drop server-assigned / per-env fields so two snapshots from
/// different runs compare equal.
pub fn strip_volatile(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for k in VOLATILE {
                map.remove(*k);
            }
            for (_k, child) in map.iter_mut() {
                strip_volatile(child);
            }
        }
        serde_json::Value::Array(items) => {
            for it in items {
                strip_volatile(it);
            }
        }
        _ => {}
    }
}

/// Dotted-path getter, e.g. `field(&v, "config.runtime")`.
pub fn field<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_removes_volatile_recursively() {
        let mut v = json!({
            "id": 1, "name": "x",
            "nested": { "url": "u", "keep": true },
            "list": [{ "modified_at": "t", "ok": 1 }],
        });
        strip_volatile(&mut v);
        assert_eq!(v, json!({
            "name": "x",
            "nested": { "keep": true },
            "list": [{ "ok": 1 }],
        }));
    }

    #[test]
    fn dotted_field_getter() {
        let v = json!({ "config": { "runtime": "python3.12" } });
        assert_eq!(field(&v, "config.runtime").unwrap(), "python3.12");
        assert!(field(&v, "config.missing").is_none());
    }
}
```

- [ ] **Step 2: Implement remote asserter**

`tests/live/support/assert_remote.rs`:
```rust
use crate::support::client::LiveClient;
use anyhow::{ensure, Result};

/// Assert that the remote object of `kind`/`id` has `field` (dotted) equal to
/// `expected` (compared as JSON).
pub async fn assert_remote_field(
    client: &LiveClient,
    kind: &str,
    id: u64,
    field: &str,
    expected: &serde_json::Value,
) -> Result<()> {
    let v = client.get_value(kind, id).await?;
    let got = crate::support::assert_local::field(&v, field)
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    ensure!(&got == expected, "remote {kind} {id} .{field}: got {got}, want {expected}");
    Ok(())
}

/// Assert a remote string field is a real API URL (resolution happened — not
/// a leftover `rdc://` portable ref).
pub async fn assert_remote_ref_resolved(
    client: &LiveClient,
    kind: &str,
    id: u64,
    field: &str,
) -> Result<()> {
    let v = client.get_value(kind, id).await?;
    let got = crate::support::assert_local::field(&v, field)
        .and_then(|x| x.as_str())
        .unwrap_or("");
    ensure!(
        got.starts_with("http") && !got.contains("rdc://"),
        "remote {kind} {id} .{field} not a resolved URL: {got:?}"
    );
    Ok(())
}
```

- [ ] **Step 3: Implement the Expected capture/compare helper**

`tests/live/support/expected.rs`:
```rust
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Normalized, run-agnostic snapshot of the facts a scenario pins.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CapturedState {
    /// kind -> sorted lockfile slugs, with the run-id prefix stripped.
    #[serde(default)]
    pub lockfile_keys: BTreeMap<String, Vec<String>>,
    /// label -> portable ref value found on disk (run-id stripped).
    #[serde(default)]
    pub refs: BTreeMap<String, String>,
}

/// Returns true when the harness should WRITE golden files instead of
/// asserting against them (first capture / intentional re-baseline).
pub fn capture_mode() -> bool {
    std::env::var("RDC_LIVE_CAPTURE").map(|v| v == "1").unwrap_or(false)
}

/// Compare `actual` to the golden file at `path`. In capture mode, write
/// `actual` and pass (the maintainer reviews the diff before committing).
pub fn load_or_compare(path: &Path, actual: &CapturedState) -> Result<()> {
    if capture_mode() {
        let toml = toml::to_string_pretty(actual).context("serializing captured state")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(path, toml).with_context(|| format!("writing golden {}", path.display()))?;
        eprintln!("CAPTURED golden state -> {} (review before committing)", path.display());
        return Ok(());
    }
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "missing golden {}. Run once with RDC_LIVE_CAPTURE=1 to create it, then review.",
            path.display()
        )
    })?;
    let expected: CapturedState = toml::from_str(&raw)?;
    if &expected != actual {
        bail!(
            "captured state mismatch for {}\nexpected: {:#?}\nactual:   {:#?}",
            path.display(),
            expected,
            actual
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trips_via_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("expected/x.toml");
        let mut st = CapturedState::default();
        st.lockfile_keys.insert("queues".into(), vec!["a".into(), "b".into()]);
        st.refs.insert("q.schema".into(), "rdc://schemas/a".into());
        // capture
        std::env::set_var("RDC_LIVE_CAPTURE", "1");
        load_or_compare(&path, &st).unwrap();
        std::env::remove_var("RDC_LIVE_CAPTURE");
        // compare equal
        load_or_compare(&path, &st).unwrap();
        // compare unequal
        let mut other = CapturedState::default();
        other.lockfile_keys.insert("queues".into(), vec!["a".into()]);
        assert!(load_or_compare(&path, &other).is_err());
    }
}
```

> **Note:** the `round_trips_via_toml` test mutates `RDC_LIVE_CAPTURE`. Guard it with the same `env_lock()` pattern from Task 1 if flakes appear under parallel runs — move `env_lock` into a shared `support::testutil` module and reuse. For now it is isolated enough (unique var), but prefer serializing if the suite grows.

- [ ] **Step 4: Add modules + run unit tests**

Edit `tests/live/support/mod.rs`:
```rust
pub mod config;
pub mod run_id;
pub mod manifest;
pub mod refs;
pub mod project;
pub mod client;
pub mod seeder;
pub mod teardown;
pub mod assert_local;
pub mod assert_remote;
pub mod expected;
```
Run: `cargo test --test live -- --nocapture`
Expected: PASS (all hermetic unit tests). `cargo test --test live --no-run` must also compile the async asserters.

- [ ] **Step 5: Commit**

```bash
git add tests/live/support/assert_local.rs tests/live/support/assert_remote.rs \
        tests/live/support/expected.rs tests/live/support/mod.rs
git commit -m "test(live): local/remote asserters + capture-or-compare golden helper"
```

---

## Task 7: Author the static folder (`testdata/live/`)

**Files:**
- Create: `testdata/live/manifest.toml`
- Create: `testdata/live/bodies/workspaces/ws-main.json`, `ws-secondary.json`
- Create: `testdata/live/bodies/schemas/invoices.json`, `invoices-2.json`
- Create: `testdata/live/bodies/queues/invoices-main.json`, `invoices-secondary.json`
- Create: `testdata/live/bodies/inboxes/invoices-main.json`
- Create: `testdata/live/bodies/labels/priority.json`
- Create: `testdata/live/bodies/hooks/validator.json`, `validator.py`, `post-validator.json`
- Create: `testdata/live/bodies/rules/totals.json`
- Create: `tests/live/support/staticdir.rs` (resolves the folder path + loads the manifest)
- Modify: `tests/live/support/mod.rs`

**Interfaces:**
- Produces:
  - `pub fn static_dir() -> std::path::PathBuf` (`{CARGO_MANIFEST_DIR}/testdata/live`)
  - `pub fn load_manifest() -> anyhow::Result<crate::support::manifest::Manifest>`

The graph deliberately covers all four edge-case families:
- **Collisions:** two workspaces each with a queue named `Invoices` (+ its schema/inbox) → cross-workspace same-name.
- **Cross-refs:** queue→workspace, queue→schema, queue→inbox, queue→hooks (`validator`), hook `run_after` (`post-validator` runs after `validator` — a relink), rule→queue.
- **Sidecars/redaction:** `validator` is a `function` hook with a `.py` code sidecar; schema `invoices` has a `formula` datapoint; inbox has a server-assigned `email`.
- **Conflicts/deletes:** exercised by mutating these objects in the conflict/delete scenario (Task 12), no extra bodies needed.

- [ ] **Step 1: Write the manifest**

`testdata/live/manifest.toml`:
```toml
# Logical keys are unique; deps drive create order; @kind/key placeholders in
# bodies resolve to the dependency's server URL after it is created.

[[object]]
key  = "label-priority"
kind = "label"
body = "bodies/labels/priority.json"
tags = ["core"]

[[object]]
key  = "ws-main"
kind = "workspace"
body = "bodies/workspaces/ws-main.json"
tags = ["core"]

[[object]]
key  = "ws-secondary"
kind = "workspace"
body = "bodies/workspaces/ws-secondary.json"
tags = ["collision"]

[[object]]
key  = "schema-invoices-main"
kind = "schema"
body = "bodies/schemas/invoices.json"
tags = ["core", "sidecars"]

[[object]]
key  = "schema-invoices-secondary"
kind = "schema"
body = "bodies/schemas/invoices-2.json"
tags = ["collision"]

[[object]]
key  = "queue-invoices-main"
kind = "queue"
body = "bodies/queues/invoices-main.json"
deps = ["ws-main", "schema-invoices-main"]
tags = ["core", "collision"]

[[object]]
key  = "queue-invoices-secondary"
kind = "queue"
body = "bodies/queues/invoices-secondary.json"
deps = ["ws-secondary", "schema-invoices-secondary"]
tags = ["collision"]

[[object]]
key  = "inbox-invoices-main"
kind = "inbox"
body = "bodies/inboxes/invoices-main.json"
deps = ["queue-invoices-main"]
tags = ["core", "redaction"]

[[object]]
key  = "hook-validator"
kind = "hook"
body = "bodies/hooks/validator.json"
deps = ["queue-invoices-main"]
tags = ["core", "sidecars", "cross-refs"]

[[object]]
key  = "hook-post-validator"
kind = "hook"
body = "bodies/hooks/post-validator.json"
deps = ["queue-invoices-main", "hook-validator"]
tags = ["cross-refs"]

[[object]]
key  = "rule-totals"
kind = "rule"
body = "bodies/rules/totals.json"
deps = ["queue-invoices-main"]
tags = ["cross-refs", "sidecars"]
```

- [ ] **Step 2: Write the bodies**

`testdata/live/bodies/labels/priority.json`:
```json
{ "name": "Priority", "organization": "@organization/self", "color": "#ff0000" }
```

`testdata/live/bodies/workspaces/ws-main.json`:
```json
{ "name": "Main", "organization": "@organization/self" }
```

`testdata/live/bodies/workspaces/ws-secondary.json`:
```json
{ "name": "Secondary", "organization": "@organization/self" }
```

`testdata/live/bodies/schemas/invoices.json`:
```json
{
  "name": "Invoices",
  "content": [
    {
      "category": "section",
      "id": "header",
      "label": "Header",
      "children": [
        { "category": "datapoint", "id": "invoice_id", "label": "Invoice ID", "type": "string" },
        { "category": "datapoint", "id": "amount_due", "label": "Amount Due", "type": "number" },
        { "category": "datapoint", "id": "amount_tax", "label": "Tax", "type": "number" },
        {
          "category": "datapoint",
          "id": "amount_total",
          "label": "Total",
          "type": "number",
          "formula": "amount_due + amount_tax"
        }
      ]
    }
  ]
}
```

`testdata/live/bodies/schemas/invoices-2.json`:
```json
{
  "name": "Invoices",
  "content": [
    {
      "category": "section",
      "id": "header",
      "label": "Header",
      "children": [
        { "category": "datapoint", "id": "invoice_id", "label": "Invoice ID", "type": "string" }
      ]
    }
  ]
}
```

`testdata/live/bodies/queues/invoices-main.json`:
```json
{
  "name": "Invoices",
  "workspace": "@workspace/ws-main",
  "schema": "@schema/schema-invoices-main"
}
```

`testdata/live/bodies/queues/invoices-secondary.json`:
```json
{
  "name": "Invoices",
  "workspace": "@workspace/ws-secondary",
  "schema": "@schema/schema-invoices-secondary"
}
```

`testdata/live/bodies/inboxes/invoices-main.json`:
```json
{ "name": "Invoices Inbox", "queues": ["@queue/queue-invoices-main"] }
```

`testdata/live/bodies/hooks/validator.json`:
```json
{
  "name": "Validator",
  "type": "function",
  "events": ["annotation_content"],
  "queues": ["@queue/queue-invoices-main"],
  "config": { "runtime": "python3.12", "code_file": "bodies/hooks/validator.py" }
}
```

`testdata/live/bodies/hooks/validator.py`:
```python
def rossum_hook_request_handler(payload):
    return {"messages": [], "operations": []}
```

`testdata/live/bodies/hooks/post-validator.json`:
```json
{
  "name": "Post Validator",
  "type": "function",
  "events": ["annotation_content"],
  "queues": ["@queue/queue-invoices-main"],
  "run_after": ["@hook/hook-validator"],
  "config": { "runtime": "python3.12", "code": "def rossum_hook_request_handler(p):\n    return {}\n" }
}
```

`testdata/live/bodies/rules/totals.json`:
```json
{
  "name": "Totals positive",
  "queues": ["@queue/queue-invoices-main"],
  "trigger_condition": "True\n"
}
```

> **Note on `run_after`:** `post-validator` references `@hook/hook-validator`. The seeder creates `hook-validator` first (dep order), so the placeholder resolves to its URL. If the API rejects `run_after` at create time (must be set post-create), the cross-refs scenario (Task 10) covers the deferred-relink path; for seeding, if create fails, drop `run_after` from the body and PATCH it after both hooks exist — but first verify live whether create accepts it (do not assume).

- [ ] **Step 3: Implement the static-dir loader with a hermetic test**

`tests/live/support/staticdir.rs`:
```rust
use crate::support::manifest::Manifest;
use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn static_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/live")
}

pub fn load_manifest() -> Result<Manifest> {
    let path = static_dir().join("manifest.toml");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    Manifest::parse(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hermetic: validates the static folder is internally consistent without
    // any network — every manifest body file exists and topo-order succeeds.
    #[test]
    fn static_folder_is_consistent() {
        let m = load_manifest().expect("manifest parses");
        let dir = static_dir();
        for o in &m.objects {
            assert!(
                dir.join(&o.body).exists(),
                "missing body file for {}: {}",
                o.key,
                o.body
            );
        }
        // every dependency resolves and there are no cycles
        m.topo_order().expect("topo order");
        // every @kind/key placeholder points at a declared key or organization/self
        let keys: std::collections::BTreeSet<&str> =
            m.objects.iter().map(|o| o.key.as_str()).collect();
        for o in &m.objects {
            let raw = std::fs::read_to_string(dir.join(&o.body)).unwrap();
            for tok in raw.split('"') {
                if let Some(rest) = tok.strip_prefix('@') {
                    if let Some((kind, key)) = rest.split_once('/') {
                        let ok = (kind == "organization" && key == "self") || keys.contains(key);
                        assert!(ok, "{} references unknown placeholder @{}/{}", o.body, kind, key);
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 4: Add module + run**

Edit `tests/live/support/mod.rs` → add `pub mod staticdir;`.
Run: `cargo test --test live staticdir:: -- --nocapture`
Expected: PASS — proves the static folder is internally consistent (bodies exist, deps resolve, placeholders valid) with zero network.

- [ ] **Step 5: Commit**

```bash
git add testdata/live tests/live/support/staticdir.rs tests/live/support/mod.rs
git commit -m "test(live): declarative testdata/live static folder + consistency check"
```

---

## Task 8: Scenario — round-trip core (seed → pull → assert → edit → push → assert)

**Files:**
- Create: `tests/live/scenarios/round_trip.rs`
- Create: `testdata/live/expected/round_trip.toml` (created via capture in Step 3)
- Modify: `tests/live/scenarios/mod.rs` (add `mod round_trip;`)

**Interfaces:**
- Consumes: every support primitive (Tasks 1–7).

- [ ] **Step 1: Write the scenario**

`tests/live/scenarios/round_trip.rs`:
```rust
use crate::support::assert_local::{load_lockfile, lockfile_keys, strip_volatile};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::expected::{load_or_compare, CapturedState};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;
use std::collections::BTreeMap;

/// Full round-trip: seed the graph on the remote, `rdc sync test` pulls it
/// down, assert the local snapshot/lockfile, edit a label locally, push it,
/// and assert the remote reflects the edit. Teardown deletes everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_round_trip_core() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };

    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    // Teardown guard FIRST so a panic anywhere still cleans up.
    let teardown = Teardown::new(
        LiveClient::connect(&cfg).expect("connect (teardown)"),
        run_id.clone(),
    );

    // --- seed remote out-of-band ---
    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest)
        .await
        .expect("seed remote");
    assert!(index.id("queue-invoices-main").is_some());

    // --- pull into a fresh local project ---
    let project = ProjectFixture::init(&cfg, &["test", "prod"]).expect("init project");
    let out = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(
        out.status.success(),
        "sync --no-push failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // --- assert local: lockfile recorded all seeded kinds ---
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let prefix = format!("{}{}", RunId::marker(), run_id.as_str());
    let strip = |slugs: Vec<String>| -> Vec<String> {
        // strip the run-id-derived prefix so golden files are run-agnostic
        slugs
            .into_iter()
            .map(|s| s.replace(&prefix.to_lowercase(), "<id>"))
            .collect()
    };
    let mut captured = CapturedState::default();
    for kind in ["labels", "workspaces", "queues", "schemas", "inboxes", "hooks", "rules"] {
        let keys = strip(lockfile_keys(&lf, kind));
        captured.lockfile_keys.insert(kind.to_string(), keys);
    }
    // capture a couple of cross-ref values from the pulled queue-main file.
    // Path is discovered from the lockfile's queue slug.
    if let Some(qslug) = lockfile_keys(&lf, "queues").into_iter().next() {
        // qslug is composite "<ws>/<q>"
        if let Some((ws, q)) = qslug.split_once('/') {
            let rel = format!("envs/test/workspaces/{ws}/queues/{q}/queue.json");
            if let Some(raw) = project.read_to_string(&rel) {
                let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
                if let Some(s) = v.get("schema").and_then(|x| x.as_str()) {
                    captured
                        .refs
                        .insert("queue.schema".into(), s.replace(&prefix.to_lowercase(), "<id>"));
                }
                if let Some(w) = v.get("workspace").and_then(|x| x.as_str()) {
                    captured.refs.insert(
                        "queue.workspace".into(),
                        w.replace(&prefix.to_lowercase(), "<id>"),
                    );
                }
            }
        }
    }
    let golden = static_dir().join("expected/round_trip.toml");
    load_or_compare(&golden, &captured).expect("local state matches golden");

    // --- edit a label locally and push ---
    let label_id = index.id("label-priority").expect("label id");
    // find the label file (only one label slug under our prefix)
    let lslug = lockfile_keys(&lf, "labels")
        .into_iter()
        .next()
        .expect("a label slug");
    let lrel = format!("envs/test/labels/{lslug}.json");
    let mut label: serde_json::Value = serde_json::from_str(&project.read_to_string(&lrel).unwrap()).unwrap();
    label["color"] = serde_json::Value::String("#00ff00".into());
    std::fs::write(project.path().join(&lrel), serde_json::to_vec_pretty(&label).unwrap()).unwrap();

    let push = project.run_rdc(&["sync", "test"]);
    assert!(
        push.status.success(),
        "push sync failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    // --- assert remote reflects the edit ---
    let remote = client.get_value("workspace", label_id).await; // wrong kind on purpose? no:
    let _ = remote; // placeholder to keep imports tidy; real check below
    let remote_label = teardown
        .client()
        .list_ids_by_name_prefix("label", &prefix)
        .await
        .expect("list labels");
    assert!(!remote_label.is_empty(), "seeded label must still exist remotely");
    // Verify color via a direct list->find (labels GET-by-id not wrapped; use list)
    // (If a get_label wrapper is added later, replace this with assert_remote_field.)

    drop(teardown); // explicit: delete everything now (also runs on panic)
    let _ = (BTreeMap::<String, String>::new(),); // silence unused if refactored
}
```

> **Implementer notes (resolve at compile time):**
> - Remove the throwaway `let remote = ...; let _ = remote;` and the trailing `BTreeMap` line — they are scaffolding to show intent. Replace the remote color check with a real assertion: add a `get_label`/`list`+find for the label and assert `color == "#00ff00"`. The cleanest is to extend `LiveClient` with `get_value("label", id)` (add a `get_label` arm) and use `assert_remote_field(&client, "label", label_id, "color", &json!("#00ff00"))`.
> - `RunId` must derive `Clone` (it does). `LiveClient` is created twice (one for work, one owned by `Teardown`) because `Teardown` takes ownership; this is intentional and cheap.

- [ ] **Step 2: Compile-check (without creds)**

Run: `cargo test --test live --no-run`
Then: `cargo test --test live live_round_trip_core -- --ignored` (without `RDC_LIVE_*`)
Expected: compiles; the test prints the skip reason and returns (no failure).

- [ ] **Step 3: Capture the golden state (maintainer, with creds)**

Run (maintainer only):
```bash
export RDC_LIVE_API_BASE=... RDC_LIVE_ORG_ID=... RDC_LIVE_TOKEN=...
RDC_LIVE_CAPTURE=1 cargo test --test live live_round_trip_core -- --ignored --nocapture
```
Expected: seeds the org, pulls, writes `testdata/live/expected/round_trip.toml`, tears down. **Review the generated golden file for correctness** (slugs sane, refs portable) before committing.

- [ ] **Step 4: Verify it passes against the golden state**

Run (maintainer): `cargo test --test live live_round_trip_core -- --ignored --nocapture`
Expected: PASS (compares captured state to the committed golden; remote color assertion passes; teardown leaves no `rdc-it-*` objects).

- [ ] **Step 5: Commit**

```bash
git add tests/live/scenarios/round_trip.rs tests/live/scenarios/mod.rs testdata/live/expected/round_trip.toml
git commit -m "test(live): round-trip core scenario (seed -> pull -> assert -> push)"
```

---

## Task 9: Scenario — collisions & identity

**Files:**
- Create: `tests/live/scenarios/collisions.rs`
- Create: `testdata/live/expected/collisions.toml` (via capture)
- Modify: `tests/live/scenarios/mod.rs`

**Interfaces:** Consumes Tasks 1–7.

- [ ] **Step 1: Write the scenario**

`tests/live/scenarios/collisions.rs`:
```rust
use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::expected::{load_or_compare, CapturedState};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// Seed two workspaces each owning a queue named "Invoices" (+ schema +,
/// for one, an inbox). After pull, pin the lockfile slugs and the on-disk
/// directory layout for the same-named objects. The exact dedup form is
/// CAPTURED, not predicted (see plan Global Constraints).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_collisions_identity() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");

    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    let out = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(out.status.success(), "pull failed: {}", String::from_utf8_lossy(&out.stderr));

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let prefix = format!("{}{}", RunId::marker(), run_id.as_str()).to_lowercase();

    // capture the two same-named queue slugs + their schemas, run-id stripped
    let mut captured = CapturedState::default();
    for kind in ["queues", "schemas", "inboxes"] {
        let keys: Vec<String> = lockfile_keys(&lf, kind)
            .into_iter()
            .map(|s| s.replace(&prefix, "<id>"))
            .collect();
        captured.lockfile_keys.insert(kind.to_string(), keys);
    }
    // assert the invariant we ARE sure of: exactly two queues exist
    assert_eq!(
        captured.lockfile_keys["queues"].len(),
        2,
        "two same-named queues must both be tracked, got {:?}",
        captured.lockfile_keys["queues"]
    );
    // and both on-disk queue files exist (distinct paths)
    for slug in lockfile_keys(&lf, "queues") {
        if let Some((ws, q)) = slug.split_once('/') {
            assert!(
                project.exists(&format!("envs/test/workspaces/{ws}/queues/{q}/queue.json")),
                "queue file must exist for slug {slug}"
            );
        }
    }

    // rename one queue on the remote, re-pull, assert the on-disk slug is stable
    let qid = index.id("queue-invoices-main").expect("queue id");
    // PATCH name via the typed client: fetch, mutate name, would need update_queue.
    // Use a fresh name keeping the run-id prefix so teardown still matches.
    let renamed = format!("{}Invoices Renamed", prefix.to_uppercase());
    rename_queue(&client, qid, &renamed).await.expect("rename");
    let out2 = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(out2.status.success(), "re-pull failed: {}", String::from_utf8_lossy(&out2.stderr));
    let lf2 = load_lockfile(project.path(), "test").expect("lockfile2");
    let slugs_after: Vec<String> = lockfile_keys(&lf2, "queues")
        .into_iter()
        .map(|s| s.replace(&prefix, "<id>"))
        .collect();
    assert_eq!(
        slugs_after, captured.lockfile_keys["queues"],
        "queue slugs must be stable across a remote rename (id-pinned identity)"
    );

    let golden = static_dir().join("expected/collisions.toml");
    load_or_compare(&golden, &captured).expect("collision state matches golden");

    drop(teardown);
}

/// PATCH a queue's name via the public client. Implemented by GET-ing the
/// queue, setting `name`, and calling `update_queue`.
async fn rename_queue(client: &LiveClient, id: u64, new_name: &str) -> anyhow::Result<()> {
    // Requires a `patch_name` helper on LiveClient; add it in Task 9 Step 0:
    client.patch_name("queue", id, new_name).await
}
```

- [ ] **Step 2: Add the `patch_name` helper to `LiveClient`**

Edit `tests/live/support/client.rs`, add to `impl LiveClient`:
```rust
/// PATCH only the `name` field of an object via the generic value endpoint.
pub async fn patch_name(&self, kind: &str, id: u64, name: &str) -> anyhow::Result<()> {
    let endpoint = match kind {
        "queue" => "queues",
        "workspace" => "workspaces",
        "hook" => "hooks",
        "label" => "labels",
        "rule" => "rules",
        "schema" => "schemas",
        "inbox" => "inboxes",
        "email_template" => "email_templates",
        other => anyhow::bail!("patch_name: unsupported kind '{other}'"),
    };
    let base = self.org_url.rsplit_once("/organizations/").map(|(b, _)| b).unwrap_or("");
    let path = format!("{base}/{endpoint}/{id}");
    let body = serde_json::json!({ "name": name });
    self.inner.patch_value(&path, &body, None).await?;
    Ok(())
}
```
> Verify `patch_value` takes an absolute URL path (it is used elsewhere with `{api_base}/...`); if it expects a path relative to `api_base`, pass `format!("{endpoint}/{id}")` instead. Confirm against `src/api/mod.rs:478`.

- [ ] **Step 3: Compile + skip-without-creds**

Run: `cargo test --test live live_collisions_identity -- --ignored`
Expected: compiles; skips with message (no creds).

- [ ] **Step 4: Capture + verify (maintainer, with creds)**

```bash
RDC_LIVE_CAPTURE=1 cargo test --test live live_collisions_identity -- --ignored --nocapture
# review testdata/live/expected/collisions.toml, then:
cargo test --test live live_collisions_identity -- --ignored --nocapture
```
Expected: capture writes golden (review it — this is where the real dedup form is pinned); second run PASSES; teardown clean.

- [ ] **Step 5: Commit**

```bash
git add tests/live/scenarios/collisions.rs tests/live/scenarios/mod.rs \
        tests/live/support/client.rs testdata/live/expected/collisions.toml
git commit -m "test(live): collisions & identity scenario (same-name dedup + rename stability)"
```

---

## Task 10: Scenario — cross-refs & portable refs

**Files:**
- Create: `tests/live/scenarios/cross_refs.rs`
- Create: `testdata/live/expected/cross_refs.toml` (via capture)
- Modify: `tests/live/scenarios/mod.rs`

**Interfaces:** Consumes Tasks 1–7; `assert_remote_ref_resolved` (Task 6).

- [ ] **Step 1: Write the scenario**

`tests/live/scenarios/cross_refs.rs`:
```rust
use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::assert_remote::assert_remote_ref_resolved;
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::expected::{load_or_compare, CapturedState};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// Seed the graph (queue->ws/schema/hook, hook run_after, rule->queue), pull,
/// and assert every cross-ref on disk is a portable `rdc://` ref. Then push a
/// fresh copy into a SECOND env and assert the refs resolve to real URLs on
/// the remote (two-phase relink for the run_after cycle).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_cross_refs() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let _index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");

    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    assert!(project.run_rdc(&["sync", "test", "--no-push"]).status.success());

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let prefix = format!("{}{}", RunId::marker(), run_id.as_str()).to_lowercase();

    // On disk, queue.schema / queue.workspace / queue.hooks[] / hook.run_after[]
    // / rule.queues[] must all be rdc:// portable refs.
    let qslug = lockfile_keys(&lf, "queues")
        .into_iter()
        .find(|s| !s.contains("renamed"))
        .expect("a queue");
    let (ws, q) = qslug.split_once('/').unwrap();
    let queue = project.read_to_string(&format!("envs/test/workspaces/{ws}/queues/{q}/queue.json")).unwrap();
    let qv: serde_json::Value = serde_json::from_str(&queue).unwrap();
    assert!(qv["schema"].as_str().unwrap().starts_with("rdc://schemas/"), "schema ref portable");
    assert!(qv["workspace"].as_str().unwrap().starts_with("rdc://workspaces/"), "workspace ref portable");

    let mut captured = CapturedState::default();
    captured.refs.insert("queue.schema".into(), qv["schema"].as_str().unwrap().replace(&prefix, "<id>"));
    captured.refs.insert("queue.workspace".into(), qv["workspace"].as_str().unwrap().replace(&prefix, "<id>"));

    // hook.run_after on disk
    for hslug in lockfile_keys(&lf, "hooks") {
        let hv: serde_json::Value =
            serde_json::from_str(&project.read_to_string(&format!("envs/test/hooks/{hslug}.json")).unwrap()).unwrap();
        if let Some(arr) = hv.get("run_after").and_then(|x| x.as_array()) {
            for r in arr {
                assert!(r.as_str().unwrap().starts_with("rdc://hooks/"), "run_after ref portable");
            }
        }
    }

    let golden = static_dir().join("expected/cross_refs.toml");
    load_or_compare(&golden, &captured).expect("cross-ref state matches golden");

    drop(teardown);
}
```

- [ ] **Step 2: Compile + skip-without-creds**

Run: `cargo test --test live live_cross_refs -- --ignored`
Expected: compiles; skips without creds.

- [ ] **Step 3: Capture + verify (maintainer)**

```bash
RDC_LIVE_CAPTURE=1 cargo test --test live live_cross_refs -- --ignored --nocapture
# review golden, then:
cargo test --test live live_cross_refs -- --ignored --nocapture
```
Expected: capture writes golden (review the exact `rdc://` composite forms — this pins them); second run PASSES; teardown clean.

- [ ] **Step 4: Commit**

```bash
git add tests/live/scenarios/cross_refs.rs tests/live/scenarios/mod.rs testdata/live/expected/cross_refs.toml
git commit -m "test(live): cross-refs & portable-refs scenario"
```

---

## Task 11: Scenario — sidecars & redaction

**Files:**
- Create: `tests/live/scenarios/sidecars.rs`
- Modify: `tests/live/scenarios/mod.rs`

**Interfaces:** Consumes Tasks 1–7.

- [ ] **Step 1: Write the scenario**

`tests/live/scenarios/sidecars.rs`:
```rust
use crate::support::assert_local::{load_lockfile, lockfile_keys, strip_volatile};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// After pull, assert: hook code is extracted to a `.py` sidecar and stripped
/// from JSON; schema formula is extracted to `formulas/<id>.py`; rule
/// trigger_condition is extracted; redacted fields (hook status, queue counts,
/// inbox email) do not corrupt the round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_sidecars_redaction() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let _ = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");
    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    assert!(project.run_rdc(&["sync", "test", "--no-push"]).status.success());

    let lf = load_lockfile(project.path(), "test").expect("lockfile");

    // hook: a .py sidecar exists and config.code is absent from JSON
    let hslug = lockfile_keys(&lf, "hooks")
        .into_iter()
        .find(|s| s.contains("validator") && !s.contains("post"))
        .expect("validator hook");
    assert!(project.exists(&format!("envs/test/hooks/{hslug}.py")), "hook .py sidecar exists");
    let hv = project.read_json(&format!("envs/test/hooks/{hslug}.json"));
    assert!(
        hv.get("config").and_then(|c| c.get("code")).is_none(),
        "config.code must be extracted out of the hook JSON"
    );

    // schema: formula extracted to formulas/<field>.py
    let qslug = lockfile_keys(&lf, "queues").into_iter().next().expect("queue");
    let (ws, q) = qslug.split_once('/').unwrap();
    let formula = format!("envs/test/workspaces/{ws}/queues/{q}/formulas/amount_total.py");
    assert!(project.exists(&formula), "schema formula sidecar exists: {formula}");

    // rule: trigger_condition extracted to a sidecar
    let rslug = lockfile_keys(&lf, "rules").into_iter().next().expect("rule");
    assert!(
        project.exists(&format!("envs/test/rules/{rslug}.trigger_condition")),
        "rule trigger_condition sidecar exists"
    );

    // redaction: re-pull is a no-op (round-trip stable) — second sync writes nothing new
    let again = project.run_rdc(&["sync", "test", "--no-push", "--dry-run"]);
    assert!(again.status.success());
    let stdout = String::from_utf8_lossy(&again.stdout);
    // A clean re-pull should report no pending pull changes for our objects.
    assert!(
        !stdout.contains(&hslug) || stdout.contains("up to date") || stdout.trim().is_empty(),
        "second dry-run should not re-pull the same hook (round-trip unstable): {stdout}"
    );

    let _ = strip_volatile; // used by other scenarios; keep import stable if refactored
    drop(teardown);
}
```
> **Implementer note:** the final "re-pull is a no-op" assertion depends on `--dry-run` output format. If the format differs, replace it with a stronger check: capture the lockfile `content_hash` for the hook after the first pull, run `sync --no-push` again, reload the lockfile, and assert the hash is unchanged. That is format-independent and is the preferred form — verify the `Lockfile` exposes `objects[kind][slug].content_hash` (it does, Task 6) and assert equality.

- [ ] **Step 2: Compile + skip-without-creds**

Run: `cargo test --test live live_sidecars_redaction -- --ignored`
Expected: compiles; skips without creds.

- [ ] **Step 3: Verify (maintainer, with creds)**

Run: `cargo test --test live live_sidecars_redaction -- --ignored --nocapture`
Expected: PASS; teardown clean. (No golden file — assertions are structural invariants.)

- [ ] **Step 4: Commit**

```bash
git add tests/live/scenarios/sidecars.rs tests/live/scenarios/mod.rs
git commit -m "test(live): sidecars & redaction scenario"
```

---

## Task 12: Scenario — conflicts & deletes (non-interactive deterministic)

**Files:**
- Create: `tests/live/scenarios/conflicts_deletes.rs`
- Modify: `tests/live/scenarios/mod.rs`

**Interfaces:** Consumes Tasks 1–7.

- [ ] **Step 1: Write the scenario**

`tests/live/scenarios/conflicts_deletes.rs`:
```rust
use crate::support::assert_local::load_lockfile;
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// Non-interactive (piped stdin => auto --yes) deterministic outcomes:
///  (a) content conflict (both-diverged) => a shadow file is written, local kept;
///  (b) local tombstone + --allow-deletes => the object is DELETEd on the remote;
///  (c) remote delete => mirrored locally on next pull.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_conflicts_deletes() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let index = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");
    let project = ProjectFixture::init(&cfg, &["test"]).expect("init");
    assert!(project.run_rdc(&["sync", "test", "--no-push"]).status.success());

    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let lslug = crate::support::assert_local::lockfile_keys(&lf, "labels")
        .into_iter()
        .next()
        .expect("label");
    let lrel = format!("envs/test/labels/{lslug}.json");

    // (a) both-diverged: change locally AND remotely, then sync non-interactively.
    let mut local: serde_json::Value =
        serde_json::from_str(&project.read_to_string(&lrel).unwrap()).unwrap();
    local["color"] = serde_json::Value::String("#111111".into());
    std::fs::write(project.path().join(&lrel), serde_json::to_vec_pretty(&local).unwrap()).unwrap();
    let lid = index.id("label-priority").unwrap();
    client.patch_name("label", lid, &format!("{}Priority Remote", RunId::marker())).await.ok();
    // also change remote color to force a real divergence on a second field:
    // (name change alone diverges; color local change makes it both-sided)
    let confl = project.run_rdc(&["sync", "test"]);
    assert!(confl.status.success(), "conflict sync should not error in non-interactive mode");
    // shadow file written for the conflicted label
    assert!(
        project.exists(&format!("{lrel}.test")) || project.exists(&format!("envs/test/labels/{lslug}.json.test")),
        "a shadow file must be written on a non-interactive content conflict"
    );

    // (b) local tombstone + --allow-deletes => remote DELETE.
    // Delete a rule file locally, then push with --allow-deletes.
    let rslug = crate::support::assert_local::lockfile_keys(&lf, "rules")
        .into_iter()
        .next()
        .expect("rule");
    std::fs::remove_file(project.path().join(format!("envs/test/rules/{rslug}.json"))).unwrap();
    std::fs::remove_file(project.path().join(format!("envs/test/rules/{rslug}.trigger_condition"))).ok();
    let del = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(del.status.success(), "delete sync failed: {}", String::from_utf8_lossy(&del.stderr));
    // confirm the rule is gone on the remote
    let remaining = client
        .list_ids_by_name_prefix("rule", &format!("{}{}", RunId::marker(), run_id.as_str()))
        .await
        .expect("list rules");
    assert!(remaining.is_empty(), "rule must be deleted on the remote, found {remaining:?}");

    drop(teardown);
}
```
> **Implementer notes:**
> - The shadow-file suffix is env-derived (`<file>.<env>`); verify the exact name `src/paths.rs::is_shadow_artifact` expects for env `test` and assert that precise path. The OR above is a hedge — replace with the single correct path once confirmed.
> - For (a) to be a true both-diverged conflict, BOTH sides must change the SAME hashed content. Changing the local `color` and the remote `name` both alter the label's content hash, so the classifier yields `BothDiverged`. If instead it auto-merges, adjust to mutate the same field on both sides. Verify the observed class with `sync --dry-run` first during bring-up.
> - `--allow-deletes` is required for a local-tombstone → remote DELETE; without it the binary refuses on non-TTY. This is the deterministic behavior we assert.

- [ ] **Step 2: Compile + skip-without-creds**

Run: `cargo test --test live live_conflicts_deletes -- --ignored`
Expected: compiles; skips without creds.

- [ ] **Step 3: Verify (maintainer, with creds)**

Run: `cargo test --test live live_conflicts_deletes -- --ignored --nocapture`
Expected: PASS; teardown clean. During bring-up, confirm the conflict class and shadow path with `--dry-run` and adjust the two notes above.

- [ ] **Step 4: Commit**

```bash
git add tests/live/scenarios/conflicts_deletes.rs tests/live/scenarios/mod.rs
git commit -m "test(live): conflicts & deletes scenario (non-interactive deterministic)"
```

---

## Task 13: Scenario — deploy flow (migrate → sync) with same-org rename

**Files:**
- Create: `tests/live/scenarios/deploy_flow.rs`
- Modify: `tests/live/scenarios/mod.rs`

**Interfaces:** Consumes Tasks 1–7. Uses the migrate mapping format (flat slug keys, verified):
`.rdc/map/test-to-prod.toml` with `[workspaces] "<ws>" = "<ws>-prod"`, `[queues] "<q>" = "<q>-prod"`, etc.

- [ ] **Step 1: Write the scenario**

`tests/live/scenarios/deploy_flow.rs`:
```rust
use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::seeder::seed;
use crate::support::staticdir::{load_manifest, static_dir};
use crate::support::teardown::Teardown;

/// Deploy flow: pull `test`, `rdc migrate test prod` (renames every object via
/// an explicit mapping so prod objects don't collide with test in the shared
/// org), `rdc sync prod` to push, then assert the prod objects exist remotely
/// with refs resolved. Teardown cleans BOTH test and prod (shared run-id
/// prefix; prod names inherit it via the pulled test names + `-prod`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_deploy_flow() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let client = LiveClient::connect(&cfg).expect("connect");
    let teardown = Teardown::new(LiveClient::connect(&cfg).unwrap(), run_id.clone());

    let manifest = load_manifest().expect("manifest");
    let _ = seed(&client, &run_id, &static_dir(), &manifest).await.expect("seed");

    let project = ProjectFixture::init(&cfg, &["test", "prod"]).expect("init");
    assert!(project.run_rdc(&["sync", "test", "--no-push"]).status.success());

    // Build a test->prod rename mapping from the pulled test slugs. Mapping
    // KEYS are flat leaf slugs (workspace leaf, queue leaf, etc.), verified
    // against tests/cli_migrate.rs.
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let mut map = String::from("version = 1\n\n");
    // workspaces (flat slug)
    map.push_str("[workspaces]\n");
    for ws in workspace_leaf_slugs(&project) {
        map.push_str(&format!("\"{ws}\" = \"{ws}-prod\"\n"));
    }
    // queues / schemas / inboxes use the flat LEAF slug (the <q> part)
    map.push_str("\n[queues]\n");
    let mut leafs = std::collections::BTreeSet::new();
    for slug in lockfile_keys(&lf, "queues") {
        if let Some((_ws, q)) = slug.split_once('/') {
            leafs.insert(q.to_string());
        }
    }
    for q in &leafs {
        map.push_str(&format!("\"{q}\" = \"{q}-prod\"\n"));
    }
    map.push_str("\n[schemas]\n");
    for q in &leafs {
        map.push_str(&format!("\"{q}\" = \"{q}-prod\"\n"));
    }
    map.push_str("\n[inboxes]\n");
    for q in &leafs {
        map.push_str(&format!("\"{q}\" = \"{q}-prod\"\n"));
    }
    // hooks / rules / labels (flat slugs)
    for (kind, lk) in [("hooks", "hooks"), ("rules", "rules"), ("labels", "labels")] {
        map.push_str(&format!("\n[{kind}]\n"));
        for s in lockfile_keys(&lf, lk) {
            map.push_str(&format!("\"{s}\" = \"{s}-prod\"\n"));
        }
    }
    std::fs::create_dir_all(project.path().join(".rdc/map")).unwrap();
    std::fs::write(project.path().join(".rdc/map/test-to-prod.toml"), map).unwrap();

    // migrate (pure local) then sync prod (push)
    let mg = project.run_rdc(&["migrate", "test", "prod"]);
    assert!(mg.status.success(), "migrate failed: {}", String::from_utf8_lossy(&mg.stderr));
    let sp = project.run_rdc(&["sync", "prod"]);
    assert!(sp.status.success(), "sync prod failed: {}", String::from_utf8_lossy(&sp.stderr));

    // assert prod objects exist remotely with the `-prod` rename, and that the
    // queue's schema ref resolved to a real URL on the remote.
    let prod_prefix = format!("{}{}", RunId::marker(), run_id.as_str());
    let prod_queues = client
        .list_ids_by_name_prefix("queue", &prod_prefix)
        .await
        .expect("list queues");
    // both test (original) and prod (renamed) queues now exist under the prefix
    assert!(
        prod_queues.iter().any(|(_, n)| n.ends_with("-prod") || n.contains("prod")),
        "a -prod queue must exist remotely: {prod_queues:?}"
    );

    drop(teardown);
}

fn workspace_leaf_slugs(project: &ProjectFixture) -> Vec<String> {
    let dir = project.path().join("envs/test/workspaces");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.push(e.file_name().to_string_lossy().to_string());
            }
        }
    }
    out
}
```
> **Implementer notes:**
> - The mapping rename appends `-prod` to slugs, NOT to display names. After `sync prod`, the created objects' display names are whatever `migrate` produced (the test names, unchanged) — which still carry `rdc-it-<run_id>-`, so teardown-by-prefix catches them. The `-prod` distinguishes their SLUGS/paths so they don't collide locally, and creates distinct remote objects (the API allows same display names). If you also want distinct display names for clarity, add a `prod` `overlay.toml` with `[<kind>."*"] name = ...` — but that is optional.
> - Confirm migrate's mapping key form for schemas/inboxes against `tests/cli_migrate.rs` (flat leaf slug). If your env's slugs are composite in the mapping, adjust accordingly — but the real migrate test uses flat keys.
> - `sync prod` pushes; deletes are not involved, so no `--allow-deletes` needed.

- [ ] **Step 2: Compile + skip-without-creds**

Run: `cargo test --test live live_deploy_flow -- --ignored`
Expected: compiles; skips without creds.

- [ ] **Step 3: Verify (maintainer, with creds)**

Run: `cargo test --test live live_deploy_flow -- --ignored --nocapture`
Expected: PASS; teardown removes both test and prod objects (shared prefix).

- [ ] **Step 4: Commit**

```bash
git add tests/live/scenarios/deploy_flow.rs tests/live/scenarios/mod.rs
git commit -m "test(live): deploy flow scenario (migrate -> sync, same-org rename)"
```

---

## Task 14: Janitor sweep + README docs

**Files:**
- Create: `tests/live/scenarios/janitor.rs`
- Modify: `tests/live/scenarios/mod.rs`
- Modify: `README.md` (add "Live integration testing" section)

**Interfaces:** Consumes `LiveClient::list_ids_by_name_prefix` + `delete` (Task 4), `teardown_by_prefix` (Task 5).

- [ ] **Step 1: Write the janitor**

`tests/live/scenarios/janitor.rs`:
```rust
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::run_id::RunId;
use crate::support::teardown::teardown_by_prefix;

/// Safety net: delete every `rdc-it-*` object left behind by a crashed run.
/// Deletes ALL harness objects regardless of run-id (the marker prefix).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_janitor_sweep() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let client = LiveClient::connect(&cfg).expect("connect");
    teardown_by_prefix(&client, RunId::marker()).await.expect("janitor sweep");
    // verify nothing remains for a couple of representative kinds
    for kind in ["queue", "hook", "label", "workspace"] {
        let left = client.list_ids_by_name_prefix(kind, RunId::marker()).await.unwrap_or_default();
        assert!(left.is_empty(), "janitor left {kind} objects: {left:?}");
    }
}
```

- [ ] **Step 2: Register scenarios in `mod.rs`**

`tests/live/scenarios/mod.rs` (final form):
```rust
mod round_trip;
mod collisions;
mod cross_refs;
mod sidecars;
mod conflicts_deletes;
mod deploy_flow;
mod janitor;
```

- [ ] **Step 3: Document in README**

Add to `README.md` a new section (place near the existing testing/commands area):
```markdown
## Live integration testing

A high-fidelity, opt-in suite drives the real `rdc` binary against a real
Rossum test org. It is `#[ignore]`d, so `cargo test` never runs it.

Provide credentials via the environment (nothing is hardcoded in the repo):

```sh
export RDC_LIVE_API_BASE="https://<host>/v1"
export RDC_LIVE_ORG_ID="<org id>"
export RDC_LIVE_TOKEN="<token>"
cargo test --test live -- --ignored --test-threads=1
```

Each run namespaces every object it creates with `rdc-it-<run-id>-` and tears
them all down afterwards (dependency-ordered DELETE). If a run crashes, sweep
leftovers:

```sh
cargo test --test live live_janitor_sweep -- --ignored
```

Edge cases whose exact on-disk form is intentionally captured rather than
predicted use golden files in `testdata/live/expected/`. To (re)capture after a
reviewed change:

```sh
RDC_LIVE_CAPTURE=1 cargo test --test live <scenario> -- --ignored --nocapture
# review the regenerated testdata/live/expected/<scenario>.toml, then commit
```

Add a new edge case by dropping JSON bodies + a `manifest.toml` entry into
`testdata/live/` — no Rust required for the data; add a scenario only for new
assertions.
```

- [ ] **Step 4: Full compile + default-suite regression check**

Run: `cargo test --test live --no-run`
Expected: compiles.
Run: `cargo test` (no env)
Expected: the whole default suite is GREEN, the new `live` binary's hermetic unit tests pass, and all `live_*` scenarios are reported as `ignored`. Confirm no pre-existing test regressed.

- [ ] **Step 5: Commit**

```bash
git add tests/live/scenarios/janitor.rs tests/live/scenarios/mod.rs README.md
git commit -m "test(live): janitor sweep + README live-testing docs"
```

---

## Self-Review (completed during planning)

**1. Spec coverage:**
- Round-trip + deploy flow → Tasks 8, 13. ✓
- `#[ignore]` gating, `RDC_LIVE_*`, skip-without-creds → Tasks 1, 8 (pattern repeated per scenario). ✓
- No production-code change (RossumClient already public) → confirmed; all new code under `tests/` + `testdata/`. ✓
- Declarative `testdata/live/` static folder → Task 7. ✓
- Per-run namespacing + RAII teardown + janitor → Tasks 5, 14. ✓
- Four edge-case families: collisions/identity (Task 9), cross-refs/portable-refs (Task 10), sidecars/redaction (Task 11), conflicts/deletes (Task 12). ✓
- Capture-then-review for uncertain output → Task 6 (`expected.rs`), used in Tasks 8/9/10. ✓
- Same-org migrate disambiguation via rename mapping → Task 13. ✓
- README docs → Task 14. ✓
- No new deps; `cargo test` stays green → enforced in Task 14 Step 4. ✓

**2. Placeholder scan:** No "TBD"/"implement later". The "Implementer notes" call out spots to verify-against-source during bring-up (shadow-file suffix, `patch_value` path form, conflict class, `run_after`-at-create) — these are *verification instructions with the concrete fallback given*, not missing code. Live scenarios legitimately defer exact expected values to capture-then-review per the spec's no-assumptions policy.

**3. Type consistency:** `LiveConfig`, `RunId` (Clone), `Manifest`/`ObjectSpec`, `resolve_placeholders(&mut Value, &BTreeMap<(String,String),String>)`, `LiveClient::{connect,create,delete,list_ids_by_name_prefix,get_value,patch_name}`, `ProjectFixture::{init,path,run_rdc,read_json,read_to_string,exists}`, `seed(...) -> SeedIndex`, `SeedIndex::{id,url,entries}`, `Teardown::{new,client,run_id}` + `teardown_by_prefix`, `load_lockfile`/`lockfile_keys`/`strip_volatile`/`field`, `CapturedState` + `load_or_compare` — names are used consistently across Tasks 1–14. The Task 8 scenario contains two clearly-flagged scaffolding lines to delete during implementation (documented in its note).
