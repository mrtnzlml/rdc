# rdc M11 — Overlays (Push Side) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce per-env `overlay.toml` files declaring env-specific values that should never live in the canonical snapshot. On push, overlay values are merged into outbound JSON payloads, so the user can keep one canonical hook + per-env overlays. After M11, the deploy story (M12) has the primitive it needs.

**Architecture:** A new `overlay` module owns the TOML schema and the apply logic. Each env optionally has `envs/<env>/overlay.toml` with a versioned model `{ version: u32, hooks: BTreeMap<slug, BTreeMap<dotted_path, Value>> }`. On push, the hooks driver loads the overlay and applies it to each hook's JSON value just before sending. Path syntax for M11 is simple dotted paths (`name`, `config.runtime`, `extra_field.nested.x`) — JMESPath wildcards/arrays defer to a later milestone. Pull-side stripping is documented as a limitation; M11 only changes push behavior.

**Tech Stack:** Same as M10. Uses existing `toml` and `serde_json` deps.

**Scope:**
- ✅ Overlay TOML format + parser
- ✅ Dotted-path apply (set value at JSON path, creating intermediate objects if missing)
- ✅ Push integration: hook overlay applied to outbound payload before PATCH
- ✅ Conflict detection coexists with overlay: hash comparison uses the post-overlay payload, so push only fires when the local content (modulo overlay) differs from base
- ❌ NOT pull-side stripping (documented limitation)
- ❌ NOT JMESPath wildcards (`hooks.*`, `arr[*]`, `arr[?id == 'x']`) — simple dotted paths only
- ❌ NOT overlay-managed key for typed fields requires special-casing — we only set into `serde_json::Value`, so typed fields just get re-set to the overlay value

**End state of M11:**

```
$ cat envs/prod/overlay.toml
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"config.runtime" = "python3.12-secure"

$ # Local hook has name "Validator (canonical)"
$ rdc push prod
# Pushes: PATCH includes name="Validator (PROD)" and config.runtime="python3.12-secure"
Pushed 1 hook to env 'prod'
```

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `src/overlay.rs` | Create | `Overlay` model, TOML load, apply-to-value, dotted-path traversal |
| `src/lib.rs` | Modify | Re-export `overlay` |
| `src/paths.rs` | Modify | Add `overlay_file()` |
| `src/cli/push/hooks.rs` | Modify | Load overlay; apply to hook JSON before computing local hash and before PATCH |
| `tests/cli_push.rs` | Modify | New test: overlay value gets sent on push |
| `README.md` | Modify | Document overlays |

---

## Task 1: `overlay` module

**Files:**
- Create: `src/overlay.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Create `src/overlay.rs`**

```rust
//! Per-env overlays — declarative env-specific values that override the
//! canonical snapshot when pushing to that env. Per spec §9.
//!
//! M11: simple dotted-path keys, push-side only. JMESPath wildcards and
//! pull-side stripping deferred to a future milestone.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Per-env overlay: declares env-specific values for one or more objects of
/// each kind. Currently only hooks are supported (since push is hooks-only as
/// of M10); other kinds will be added when their push paths land.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Overlay {
    pub version: u32,
    /// Hook overlays keyed by slug. Inner map is dotted-path → value.
    #[serde(default)]
    pub hooks: BTreeMap<String, BTreeMap<String, Value>>,
}

impl Overlay {
    /// Load an overlay from disk. Returns `Ok(None)` if the file does not
    /// exist, `Err` on parse failure.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let overlay: Overlay = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(overlay))
    }

    /// Get the overlay map for a single hook, if any.
    pub fn hook(&self, slug: &str) -> Option<&BTreeMap<String, Value>> {
        self.hooks.get(slug)
    }
}

/// Apply a flat dotted-path → value map onto a `serde_json::Value`. Creates
/// intermediate objects if missing. Existing values at the path are
/// overwritten unconditionally. Used by push drivers right before sending.
pub fn apply_overrides(value: &mut Value, overrides: &BTreeMap<String, Value>) {
    for (path, new_value) in overrides {
        set_at_path(value, path, new_value.clone());
    }
}

fn set_at_path(value: &mut Value, path: &str, new_value: Value) {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.is_empty() {
        return;
    }
    let mut current = value;
    for segment in &segments[..segments.len() - 1] {
        // Walk into / create an object at this segment.
        if !current.is_object() {
            // The parent slot must be an object. If it's null/array/scalar,
            // replace with empty object so we can recurse.
            *current = Value::Object(Default::default());
        }
        let obj = current.as_object_mut().expect("just made object");
        let entry = obj.entry((*segment).to_string()).or_insert(Value::Object(Default::default()));
        if !entry.is_object() {
            *entry = Value::Object(Default::default());
        }
        current = entry;
    }
    if !current.is_object() {
        *current = Value::Object(Default::default());
    }
    let obj = current.as_object_mut().expect("just made object");
    obj.insert(segments.last().unwrap().to_string(), new_value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn load_returns_none_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");
        let res = Overlay::load(&path).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn load_parses_valid_overlay() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overlay.toml");
        std::fs::write(&path, r#"
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"config.runtime" = "python3.12-secure"
"#).unwrap();
        let overlay = Overlay::load(&path).unwrap().unwrap();
        assert_eq!(overlay.version, 1);
        let hook = overlay.hook("validator-invoices").unwrap();
        assert_eq!(hook.get("name").unwrap(), &Value::String("Validator (PROD)".into()));
        assert_eq!(hook.get("config.runtime").unwrap(), &Value::String("python3.12-secure".into()));
    }

    #[test]
    fn apply_simple_top_level_override() {
        let mut v = json!({ "name": "Original", "id": 1 });
        let mut overrides = BTreeMap::new();
        overrides.insert("name".to_string(), Value::String("Override".into()));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["name"], Value::String("Override".into()));
        assert_eq!(v["id"], Value::Number(1.into()));
    }

    #[test]
    fn apply_nested_dotted_override() {
        let mut v = json!({ "config": { "runtime": "old", "other": "kept" } });
        let mut overrides = BTreeMap::new();
        overrides.insert("config.runtime".to_string(), Value::String("new".into()));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["config"]["runtime"], Value::String("new".into()));
        assert_eq!(v["config"]["other"], Value::String("kept".into()));
    }

    #[test]
    fn apply_creates_intermediate_objects_when_missing() {
        let mut v = json!({ "name": "x" });
        let mut overrides = BTreeMap::new();
        overrides.insert("settings.deep.value".to_string(), Value::String("created".into()));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["settings"]["deep"]["value"], Value::String("created".into()));
        assert_eq!(v["name"], Value::String("x".into()));
    }

    #[test]
    fn apply_replaces_non_object_at_intermediate_path() {
        // If "config" is a string and we want to set config.runtime, the
        // string is replaced with an object.
        let mut v = json!({ "config": "scalar" });
        let mut overrides = BTreeMap::new();
        overrides.insert("config.runtime".to_string(), Value::String("py".into()));
        apply_overrides(&mut v, &overrides);
        assert_eq!(v["config"]["runtime"], Value::String("py".into()));
    }
}
```

- [ ] **Step 2: Re-export from `src/lib.rs`**

In `src/lib.rs`, add `pub mod overlay;` (alphabetical position):

```rust
pub mod api;
pub mod cli;
pub mod config;
pub mod model;
pub mod overlay;
pub mod paths;
pub mod secrets;
pub mod slug;
pub mod snapshot;
pub mod state;
```

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib overlay`
Expected: 5 overlay tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/overlay.rs
git commit -m "feat(overlay): TOML overlay model + dotted-path apply helper"
```

---

## Task 2: `paths.overlay_file()` accessor

**Files:**
- Modify: `src/paths.rs`

- [ ] **Step 1: Add the accessor + test**

In `src/paths.rs`, inside `impl Paths` (just after `organization_file` for proximity to other env-root files):

```rust
    /// `<root>/envs/<env>/overlay.toml`
    pub fn overlay_file(&self) -> PathBuf {
        self.env_root().join("overlay.toml")
    }
```

In `mod tests`:

```rust
    #[test]
    fn overlay_file_path() {
        assert_eq!(p().overlay_file(), Path::new("/proj/envs/dev/overlay.toml"));
    }
```

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib paths`
Expected: existing paths tests + 1 new = total +1.

- [ ] **Step 3: Commit**

```bash
git add src/paths.rs
git commit -m "feat(paths): overlay_file accessor"
```

---

## Task 3: Apply overlay in push hooks driver

**Files:**
- Modify: `src/cli/push/hooks.rs`

- [ ] **Step 1: Update the push driver to apply overlay before PATCH**

Replace `src/cli/push/hooks.rs`:

```rust
use crate::api::RossumClient;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::snapshot::hook::{read_hook, serialize_hook};
use crate::state::{hook_combined_hash, Lockfile, ObjectEntry};
use anyhow::{Context, Result};

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
) -> Result<(usize, usize)> {
    let hooks_dir = paths.hooks_dir();
    if !hooks_dir.exists() {
        return Ok((0, 0));
    }

    // Load overlay if present.
    let overlay = Overlay::load(&paths.overlay_file())
        .with_context(|| format!("loading overlay from {}", paths.overlay_file().display()))?;

    let mut pushed = 0usize;
    let mut skipped = 0usize;

    let entries: Vec<_> = std::fs::read_dir(&hooks_dir)
        .with_context(|| format!("reading {}", hooks_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing {}", hooks_dir.display()))?;

    let mut remote_hooks: Option<Vec<crate::model::Hook>> = None;

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(slug) = name.strip_suffix(".json") else { continue };
        if slug.ends_with(".remote") {
            continue;
        }

        // Read local hook.
        let local_hook = read_hook(&hooks_dir, slug)
            .with_context(|| format!("reading local hook '{slug}'"))?;

        // Build the JSON value that we'd PATCH (with overlay applied), and
        // also track its post-extraction bytes for hash comparison.
        let mut payload = serde_json::to_value(&local_hook)
            .context("serializing local hook to value")?;
        if let Some(ov) = &overlay {
            if let Some(hook_overrides) = ov.hook(slug) {
                apply_overrides(&mut payload, hook_overrides);
            }
        }

        // Reconstruct a Hook from the overlay-applied payload to pass to
        // update_hook (which takes &Hook).
        let payload_hook: crate::model::Hook = serde_json::from_value(payload.clone())
            .with_context(|| format!("re-deserializing overlay-applied hook '{slug}'"))?;

        // Compute the local-after-overlay combined hash.
        let (post_overlay_json, post_overlay_code) = serialize_hook(&payload_hook)?;
        let local_combined = hook_combined_hash(&post_overlay_json, &post_overlay_code);

        let entry = lockfile.objects.get("hooks").and_then(|m| m.get(slug));
        let Some(entry) = entry else {
            eprintln!("warning: hooks/{slug}.json — no lockfile entry, skipping (creates not supported in M10)");
            skipped += 1;
            continue;
        };

        let Some(base) = &entry.content_hash else {
            eprintln!("warning: hooks/{slug}.json — lockfile entry has no content_hash, skipping");
            skipped += 1;
            continue;
        };

        if &local_combined == base {
            // No effective change after overlay applied.
            continue;
        }

        let id = entry.id;

        // Lazy-fetch remote.
        if remote_hooks.is_none() {
            remote_hooks = Some(
                client.list_hooks().await
                    .context("listing hooks to verify no drift before push")?,
            );
        }
        let remote_list = remote_hooks.as_ref().unwrap();
        let Some(remote_hook) = remote_list.iter().find(|h| h.id == id) else {
            eprintln!("warning: hooks/{slug}.json — id {id} not found on remote, skipping");
            skipped += 1;
            continue;
        };

        let (remote_json, remote_code) = serialize_hook(remote_hook)?;
        let remote_combined = hook_combined_hash(&remote_json, &remote_code);

        if &remote_combined != base {
            eprintln!(
                "warning: hooks/{slug}.json — remote has changed since last pull, skipping push (run `rdc pull` first)"
            );
            skipped += 1;
            continue;
        }

        // Send the overlay-applied payload.
        let updated = client.update_hook(id, &payload_hook).await
            .with_context(|| format!("PATCH /hooks/{id}"))?;

        // Update lockfile from server response. The server's response IS the
        // canonical state; future pulls will see this hash.
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

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests still pass — adding overlay support doesn't change behavior when no overlay file exists.

- [ ] **Step 3: Commit**

```bash
git add src/cli/push/hooks.rs
git commit -m "feat(cli): apply overlay to outbound payload on push"
```

---

## Task 4: Integration test for overlay-on-push

**Files:**
- Modify: `tests/cli_push.rs`

- [ ] **Step 1: Add overlay test**

Append to `tests/cli_push.rs`:

```rust
/// Push with an overlay sends the overlay-applied payload (overlay overrides
/// the local snapshot value). Verified by capturing the PATCH body via a
/// custom matcher.
#[tokio::test]
async fn push_applies_overlay_values_to_outbound_patch() {
    use std::sync::{Arc, Mutex};

    let server = MockServer::start().await;
    mount_get_only_hooks_org(&server, fixture("hooks_list.json")).await;

    // Capture the PATCH body so we can assert the overlay was applied.
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();

    Mock::given(method("PATCH"))
        .and(path("/api/v1/hooks/1"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            *captured_clone.lock().unwrap() = Some(body.clone());
            // Echo body back as response (server would normally normalize but
            // for this test it's fine to return what we got).
            ResponseTemplate::new(200).set_body_json(body)
        })
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

    // Edit the .py file so push has something to send.
    let py_path = project.path().join("envs/dev/hooks/validator-invoices.py");
    let original = std::fs::read_to_string(&py_path).unwrap();
    std::fs::write(&py_path, format!("{original}# local edit\n")).unwrap();

    // Add an overlay declaring an env-specific name.
    let overlay_path = project.path().join("envs/dev/overlay.toml");
    std::fs::write(&overlay_path, r#"
version = 1

[hooks.validator-invoices]
"name" = "Validator (DEV-OVERLAY)"
"config.runtime" = "python3.12-overlay"
"#).unwrap();

    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["push", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("Pushed 1 hook"));

    let body = captured.lock().unwrap().clone().expect("PATCH body should be captured");
    assert_eq!(body["name"], serde_json::Value::String("Validator (DEV-OVERLAY)".into()));
    assert_eq!(body["config"]["runtime"], serde_json::Value::String("python3.12-overlay".into()));
}
```

- [ ] **Step 2: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests pass — adds 1 new push test.

- [ ] **Step 3: Commit**

```bash
git add tests/cli_push.rs
git commit -m "test(cli): integration test for overlay-on-push"
```

---

## Task 5: README update

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update Status + add Overlays section**

Update Status:
```
**Status:** M11. Pull side feature-complete. `rdc push` for hooks with optional per-env overlays.
```

Add a new "Overlays" section after "Push":

```
## Overlays (M11 — push side, hooks only)

`envs/<env>/overlay.toml` declares values that should always be set when
pushing to that env, regardless of the canonical snapshot. Useful for
per-env names, secrets, URLs.

```toml
version = 1

[hooks.validator-invoices]
"name" = "Validator (PROD)"
"config.runtime" = "python3.12-secure"
```

On `rdc push`, the overlay's dotted-path keys are merged into the outbound
PATCH body, overwriting any value at that path. The overlay is the source
of truth for declared keys; manual edits to those keys in the snapshot are
overwritten by the overlay on push.

**M11 limitations:**
- Hooks only (matches push scope).
- Push-side only — pull does not strip overlay-managed values yet.
- Simple dotted paths only; no JMESPath wildcards or array filters.
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: M11 — overlays for push"
```

---

## Self-Review

**Spec coverage:**
- §9 Overlay model — partial: format implemented, push apply implemented, pull strip deferred.

**Placeholder scan:** No "TBD"/"TODO" patterns.

**Type consistency:** `Overlay { version, hooks }` consistent in Tasks 1, 3. `apply_overrides(&mut Value, &BTreeMap<String, Value>)` consistent.

**Scope check:** 5 tasks. Pure additive — overlay is opt-in (file presence triggers behavior).

---

## Next milestones

- **M12:** Mapping wizard, `rdc plan`, `rdc apply` — TEST→PROD deploy workflow built on overlays.
- **M13:** Push extension to remaining kinds (queues, schemas, rules…); pull-side overlay stripping; auxiliary commands.
- **M14:** Distribution.
