# Store Extensions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Rossum store-installed extensions (Master Data Hub, Email Notifications, etc.) work end-to-end in `rdc pull`, `rdc push`, `rdc deploy`, `rdc status` — closing the create-side gap (the existing single-cluster install endpoint `POST /hooks/create` is mandatory; standard `POST /hooks/` 400s) without disrupting regular-hook flow.

**Architecture:** Detect store extensions by `extension_source == "rossum_store"` on the existing `Hook` model via accessor methods; route same-env create through a two-call (`/hooks/create` + `PATCH`) flow with automatic orphan recovery; resolve cross-cluster `hook_template` URLs by `(name, type, source)` match against the target cluster (cached in `.rdc/map/<src>→<tgt>.toml`); resolve per-env `token_owner` via overlay (auto-populated by interactive prompt on first deploy).

**Tech Stack:** Rust (existing rdc codebase), serde/serde_json, reqwest, wiremock for tests, toml for config. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-05-13-store-extensions-design.md` — read it before starting.

---

## File Structure

**Create:**
- `src/model/hook_template.rs` — `HookTemplate` struct mirroring `/api/v1/hook_templates/<id>` response shape (round-trip-fidelity via `extra` flatten, like every other model).
- `src/model/user.rs` — lightweight `User` struct (id, url, username, first_name, last_name, is_active, groups). Only the fields the picker needs; the rest goes into `extra`.
- `src/cli/deploy/store_extensions.rs` — encapsulates template-URL resolution (`(name, type, source) → tgt_url`), the `[hook_templates]` cache I/O, the interactive `token_owner` picker, and the helper that decides "is this snapshot hook a store extension?". Lives under `deploy/` because the cross-env logic is the bulk; same-env push reuses the detection helper and the install-payload builder from here.
- `testdata/fixtures/store_extensions/hook_mdh.json` — full server response for an installed MDH hook (captured during the design session).
- `testdata/fixtures/store_extensions/hook_templates_list.json` — `/api/v1/hook_templates` list response (subset is fine).

**Modify:**
- `src/model/mod.rs` — `pub mod` the two new model files; re-export `HookTemplate`, `User`.
- `src/model/hook.rs` — add `extension_source()`, `hook_template()`, `is_store_extension()` accessor methods.
- `src/api/mod.rs` — three new methods: `create_hook_via_install`, `list_hook_templates`, `list_users`.
- `src/mapping.rs` — add `hook_templates: BTreeMap<String, String>` field on `Mapping`.
- `src/overlay.rs` — add `defaults: Defaults` field on `Overlay`; new `Defaults` struct with one field `store_extension_token_owner: Option<String>`.
- `src/cli/deploy/common.rs::rewrite_urls` — accept an additional `explicit_subs: &BTreeMap<String, String>` parameter that takes precedence over the lockfile/mapping path.
- `src/cli/push/hooks.rs` — when the local hook is a store extension and lockfile entry is absent, dispatch to the two-call install path (orphan-check, POST `/hooks/create`, PATCH).
- `src/cli/deploy/create.rs::create_hook` — same dispatch on the deploy bootstrap path. Use the in-memory template index to remap `hook_template`; use the resolved tgt `token_owner` from overlay.
- `src/cli/deploy/apply.rs` — pass the template-URL substitution map into `rewrite_urls` calls. Hooks-loop unchanged otherwise (PATCH sweep already round-trips store fields).
- `src/cli/deploy/run.rs` (or wherever the deploy entry point invokes `apply::run`) — before bootstrap, resolve template URLs and run the interactive token_owner prompt for any store hook missing one.
- `src/cli/status.rs` — surface "N store extensions" on the lockfile summary line when non-zero.
- `src/cli/resolve.rs` — new `pick_token_owner` interactive helper (modeled on `resolve_push_drift`).
- `tests/cli_push.rs` — end-to-end wiremock test for store-extension install + customize cycle, including orphan recovery.
- `tests/cli_deploy.rs` — end-to-end wiremock test for cross-env bootstrap (template resolution + token_owner overlay write + two-call create).

---

## Task 1: Hook accessors for store-extension detection

**Files:**
- Modify: `src/model/hook.rs`

- [ ] **Step 1: Add the failing test inside `src/model/hook.rs`'s `mod tests` block**

```rust
#[test]
fn extension_source_reads_from_extra() {
    let payload = json!({
        "id": 1, "url": "u", "name": "n", "type": "webhook",
        "extension_source": "rossum_store",
        "hook_template": "https://x/api/v1/hook_templates/39"
    });
    let h: Hook = serde_json::from_value(payload).unwrap();
    assert_eq!(h.extension_source(), Some("rossum_store"));
    assert_eq!(h.hook_template(), Some("https://x/api/v1/hook_templates/39"));
    assert!(h.is_store_extension());
}

#[test]
fn extension_source_custom_is_not_store_extension() {
    let payload = json!({
        "id": 1, "url": "u", "name": "n", "type": "function",
        "extension_source": "custom",
        "hook_template": Value::Null
    });
    let h: Hook = serde_json::from_value(payload).unwrap();
    assert_eq!(h.extension_source(), Some("custom"));
    assert_eq!(h.hook_template(), None);
    assert!(!h.is_store_extension());
}

#[test]
fn extension_source_absent_is_none() {
    let payload = json!({"id": 1, "url": "u", "name": "n", "type": "function"});
    let h: Hook = serde_json::from_value(payload).unwrap();
    assert_eq!(h.extension_source(), None);
    assert!(!h.is_store_extension());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rdc --lib model::hook::tests::extension_source_ -- --nocapture`
Expected: FAIL (methods don't exist yet).

- [ ] **Step 3: Implement the accessors on `impl Hook`**

In `src/model/hook.rs`, inside `impl Hook { ... }`, alongside `modified_at`:

```rust
/// Returns `extension_source` if present and a string. Round-trip-safe:
/// always read from `extra` (the field is serialised verbatim).
pub fn extension_source(&self) -> Option<&str> {
    self.extra.get("extension_source").and_then(|v| v.as_str())
}

/// Returns `hook_template` (a URL) if present and a string. For regular
/// hooks this is `null` on the wire and yields `None` here.
pub fn hook_template(&self) -> Option<&str> {
    self.extra.get("hook_template").and_then(|v| v.as_str())
}

/// True iff this hook came from the Rossum store and must be created via
/// `POST /hooks/create` rather than the regular `POST /hooks/`.
pub fn is_store_extension(&self) -> bool {
    self.extension_source() == Some("rossum_store")
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --lib model::hook`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/model/hook.rs
git commit -m "hook: add extension_source/hook_template/is_store_extension accessors"
```

---

## Task 2: HookTemplate model

**Files:**
- Create: `src/model/hook_template.rs`
- Modify: `src/model/mod.rs`

- [ ] **Step 1: Add the failing test in a fresh `src/model/hook_template.rs`**

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct HookTemplate {
    #[serde(default)]
    pub url: String,
    pub name: String,
    #[serde(rename = "type")]
    pub template_type: String,
    pub extension_source: String,
    pub install_action: String,
    /// Forward-compat: any field not modelled here survives via round-trip.
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
            "url": "https://elis.rossum.ai/api/v1/hook_templates/39",
            "name": "Master Data Hub",
            "type": "webhook",
            "extension_source": "rossum_store",
            "install_action": "copy",
            "description": "Enhance ...",
            "guide": "<div>...</div>",
            "settings_schema": null
        });
        let t: HookTemplate = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(t.name, "Master Data Hub");
        assert_eq!(t.template_type, "webhook");
        assert_eq!(t.install_action, "copy");
        let round_trip = serde_json::to_value(&t).unwrap();
        assert_eq!(round_trip, payload);
    }
}
```

- [ ] **Step 2: Wire the module into `src/model/mod.rs`**

Add (next to the existing `pub mod hook;` etc.):

```rust
pub mod hook_template;
pub use hook_template::HookTemplate;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p rdc --lib model::hook_template`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/model/hook_template.rs src/model/mod.rs
git commit -m "model: add HookTemplate"
```

---

## Task 3: Lightweight User model

**Files:**
- Create: `src/model/user.rs`
- Modify: `src/model/mod.rs`

- [ ] **Step 1: Add the failing test in a fresh `src/model/user.rs`**

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct User {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub groups: Vec<String>,
    /// Forward-compat: every field not modelled survives via round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl User {
    /// True iff the user is in the admin group (`/api/v1/groups/3`). The
    /// admin group is the canonical role for store-extension service
    /// accounts; the picker filters / ranks by this.
    pub fn is_admin(&self) -> bool {
        self.groups.iter().any(|g| g.ends_with("/groups/3"))
    }

    /// True iff this user looks like an auto-provisioned system account.
    /// Convention is `system_user__<hash>`; deleted variants get a
    /// `_deleted_*` suffix and are filtered out by `is_active=false`.
    pub fn is_system_user(&self) -> bool {
        self.username.starts_with("system_user__")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_minimal_response() {
        let payload = json!({
            "id": 938493,
            "url": "https://elis.rossum.ai/api/v1/users/938493",
            "username": "system_user__a556534d",
            "first_name": "SYSTEM USER",
            "last_name": "(DO NOT DELETE)",
            "is_active": true,
            "groups": ["https://elis.rossum.ai/api/v1/groups/3"]
        });
        let u: User = serde_json::from_value(payload).unwrap();
        assert_eq!(u.id, 938493);
        assert!(u.is_admin());
        assert!(u.is_system_user());
    }

    #[test]
    fn non_admin_user() {
        let payload = json!({
            "id": 1, "url": "u", "username": "alice",
            "is_active": true, "groups": ["https://x/api/v1/groups/2"]
        });
        let u: User = serde_json::from_value(payload).unwrap();
        assert!(!u.is_admin());
        assert!(!u.is_system_user());
    }
}
```

- [ ] **Step 2: Wire into `src/model/mod.rs`**

```rust
pub mod user;
pub use user::User;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p rdc --lib model::user`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/model/user.rs src/model/mod.rs
git commit -m "model: add lightweight User (id/url/username/groups + is_admin/is_system_user)"
```

---

## Task 4: `create_hook_via_install` API method

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `tests/api.rs`

- [ ] **Step 1: Add the failing wiremock test in `tests/api.rs`**

Follow the existing tests' shape (wiremock `MockServer`, `RossumClient::new` against `mock_server.uri()`). Append:

```rust
use rdc::model::Hook;
use serde_json::json;
use wiremock::{matchers::{method, path, header, body_json}, Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn create_hook_via_install_posts_to_create_endpoint() {
    let server = MockServer::start().await;
    let install_body = json!({
        "name": "Master Data Hub",
        "hook_template": "https://elis.rossum.ai/api/v1/hook_templates/39",
        "events": ["annotation_content.initialize"],
        "queues": [],
        "token_owner": "https://elis.rossum.ai/api/v1/users/938493"
    });
    let server_response = json!({
        "id": 1798871,
        "url": format!("{}/hooks/1798871", server.uri()),
        "name": "Master Data Hub",
        "type": "webhook",
        "events": ["annotation_content.initialize"],
        "queues": [],
        "config": { "private": true, "timeout_s": 60 },
        "settings": { "configurations": [] },
        "extension_source": "rossum_store",
        "hook_template": "https://elis.rossum.ai/api/v1/hook_templates/39",
        "token_owner": "https://elis.rossum.ai/api/v1/users/938493"
    });
    Mock::given(method("POST"))
        .and(path("/hooks/create"))
        .and(header("authorization", "token TKN"))
        .and(body_json(&install_body))
        .respond_with(ResponseTemplate::new(201).set_body_json(&server_response))
        .mount(&server)
        .await;

    let client = rdc::api::RossumClient::new(server.uri(), "TKN".into()).unwrap();
    let hook: Hook = client.create_hook_via_install(&install_body, None).await.unwrap();
    assert_eq!(hook.id, 1798871);
    assert_eq!(hook.extension_source(), Some("rossum_store"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test api create_hook_via_install_posts_to_create_endpoint`
Expected: FAIL ("no method named `create_hook_via_install`").

- [ ] **Step 3: Implement the method in `src/api/mod.rs`**

Add next to the existing `create_hook`:

```rust
/// POST `/hooks/create` — the Rossum store install endpoint. Unlike
/// `create_hook` (which posts to `/hooks/`), this accepts a minimal body
/// `{name, hook_template, events, queues, token_owner}` and the server
/// fills in the rest from the referenced template (per the template's
/// `install_action: "copy"`). Required for store extensions because
/// `POST /hooks/` rejects them with 400 (`config.url` is required for
/// webhook-type hooks, but store webhooks have `config.private: true`
/// and no URL).
pub async fn create_hook_via_install(
    &self,
    body: &serde_json::Value,
    progress: ProgressHandle,
) -> Result<Hook> {
    self.post_json("/hooks/create", body, progress).await
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --test api create_hook_via_install_posts_to_create_endpoint`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/api/mod.rs tests/api.rs
git commit -m "api: add create_hook_via_install (POST /hooks/create) for store extensions"
```

---

## Task 5: `list_hook_templates` API method

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `tests/api.rs`

- [ ] **Step 1: Add the failing test**

```rust
#[tokio::test]
async fn list_hook_templates_paginates() {
    use rdc::model::HookTemplate;
    let server = MockServer::start().await;
    let page1 = json!({
        "pagination": { "next": format!("{}/hook_templates?page=2", server.uri()) },
        "results": [
            {"url": format!("{}/hook_templates/39", server.uri()),
             "name": "Master Data Hub", "type": "webhook",
             "extension_source": "rossum_store", "install_action": "copy"}
        ]
    });
    let page2 = json!({
        "pagination": { "next": null },
        "results": [
            {"url": format!("{}/hook_templates/27", server.uri()),
             "name": "Email Notifications", "type": "webhook",
             "extension_source": "rossum_store", "install_action": "copy"}
        ]
    });
    Mock::given(method("GET"))
        .and(path("/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&page1))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/hook_templates"))
        .and(wiremock::matchers::query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&page2))
        .mount(&server).await;

    let client = rdc::api::RossumClient::new(server.uri(), "TKN".into()).unwrap();
    let templates: Vec<HookTemplate> = client.list_hook_templates(None).await.unwrap();
    assert_eq!(templates.len(), 2);
    assert!(templates.iter().any(|t| t.name == "Master Data Hub"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test api list_hook_templates_paginates`
Expected: FAIL ("no method named `list_hook_templates`").

- [ ] **Step 3: Implement in `src/api/mod.rs`**

Add to the list of `list_*` methods (after `list_email_templates`):

```rust
pub async fn list_hook_templates(
    &self,
    progress: ProgressHandle,
) -> Result<Vec<crate::model::HookTemplate>> {
    self.list_paginated("/hook_templates", progress).await
}
```

(`list_paginated` already handles pagination and the `Page<T>`/`Pagination` types.)

- [ ] **Step 4: Run the test**

Run: `cargo test --test api list_hook_templates_paginates`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/api/mod.rs tests/api.rs
git commit -m "api: list_hook_templates (paginated GET /hook_templates)"
```

---

## Task 6: `list_users` API method

**Files:**
- Modify: `src/api/mod.rs`
- Modify: `tests/api.rs`

- [ ] **Step 1: Failing test**

```rust
#[tokio::test]
async fn list_users_paginates() {
    use rdc::model::User;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
            "pagination": { "next": null },
            "results": [
                {"id": 938493, "url": format!("{}/users/938493", server.uri()),
                 "username": "system_user__a556534d", "is_active": true,
                 "groups": [format!("{}/groups/3", server.uri())]},
                {"id": 200001, "url": format!("{}/users/200001", server.uri()),
                 "username": "alice@example.org", "is_active": true,
                 "groups": [format!("{}/groups/3", server.uri())]}
            ]
        })))
        .mount(&server).await;

    let client = rdc::api::RossumClient::new(server.uri(), "TKN".into()).unwrap();
    let users: Vec<User> = client.list_users(None).await.unwrap();
    assert_eq!(users.len(), 2);
    assert!(users.iter().any(|u| u.is_system_user()));
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test --test api list_users_paginates`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
pub async fn list_users(
    &self,
    progress: ProgressHandle,
) -> Result<Vec<crate::model::User>> {
    self.list_paginated("/users", progress).await
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test --test api list_users_paginates`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/api/mod.rs tests/api.rs
git commit -m "api: list_users (paginated GET /users)"
```

---

## Task 7: `[hook_templates]` section in mapping TOML

**Files:**
- Modify: `src/mapping.rs`

- [ ] **Step 1: Failing test in `src/mapping.rs::tests`**

```rust
#[test]
fn hook_templates_section_round_trips() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test_to_prod.toml");
    let mut m = Mapping::default();
    m.hook_templates.insert(
        "https://test.rossum.app/api/v1/hook_templates/39".into(),
        "https://prod.rossum.app/api/v1/hook_templates/41".into(),
    );
    m.save(&path).unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("[hook_templates]"));
    assert!(raw.contains("hook_templates/39"));

    let loaded = Mapping::load(&path).unwrap();
    assert_eq!(loaded, m);
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p rdc --lib mapping::tests::hook_templates_section_round_trips`
Expected: FAIL.

- [ ] **Step 3: Add the field to `Mapping` and `Default`**

In `src/mapping.rs`:

```rust
// In the struct, after `engine_fields`:
/// Cross-cluster hook_template URL pairs, built by `rdc deploy` on first
/// run and persisted so subsequent deploys don't re-list `/hook_templates`
/// on the target cluster. Hand-editable for forced overrides.
#[serde(default)]
pub hook_templates: BTreeMap<String, String>,
```

```rust
// In `impl Default for Mapping`, add:
hook_templates: BTreeMap::new(),
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p rdc --lib mapping`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/mapping.rs
git commit -m "mapping: add [hook_templates] section for cross-cluster template URL pairs"
```

---

## Task 8: `[defaults]` section in overlay TOML

**Files:**
- Modify: `src/overlay.rs`

- [ ] **Step 1: Failing test in `src/overlay.rs::tests`**

```rust
#[test]
fn defaults_section_parses_store_extension_token_owner() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("overlay.toml");
    std::fs::write(&path, r#"
version = 1

[defaults]
store_extension_token_owner = "https://prod/api/v1/users/938493"

[hooks.master-data-hub]
"name" = "MDH (PROD)"
"#).unwrap();
    let overlay = Overlay::load(&path).unwrap().unwrap();
    assert_eq!(
        overlay.defaults.store_extension_token_owner.as_deref(),
        Some("https://prod/api/v1/users/938493")
    );
    assert!(overlay.hook("master-data-hub").is_some());
}

#[test]
fn defaults_section_is_optional() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("overlay.toml");
    std::fs::write(&path, "version = 1\n").unwrap();
    let overlay = Overlay::load(&path).unwrap().unwrap();
    assert!(overlay.defaults.store_extension_token_owner.is_none());
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p rdc --lib overlay::tests::defaults_section_`
Expected: FAIL ("no field `defaults`").

- [ ] **Step 3: Implement**

In `src/overlay.rs`, add the new struct above `Overlay`:

```rust
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct Defaults {
    /// Fallback `token_owner` URL applied to every store extension that
    /// has no per-hook `token_owner` override. Set automatically by
    /// `rdc deploy`'s interactive picker on first deploy; hand-editable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_extension_token_owner: Option<String>,
}
```

Add to `Overlay`:

```rust
#[serde(default)]
pub defaults: Defaults,
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rdc --lib overlay`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/overlay.rs
git commit -m "overlay: add [defaults] section with store_extension_token_owner"
```

---

## Task 9: Extend `rewrite_urls` with explicit substitutions

**Files:**
- Modify: `src/cli/deploy/common.rs`
- Modify: callers in `src/cli/deploy/apply.rs` and `src/cli/deploy/create.rs`

The new `explicit_subs` map is the deploy-time mechanism for substituting URLs that don't live in the lockfile — primarily `hook_template` (resolved by name match on the target cluster). It takes precedence over the lockfile-based path.

- [ ] **Step 1: Failing test in `src/cli/deploy/common.rs::tests`**

```rust
#[test]
fn rewrite_urls_explicit_subs_take_precedence() {
    let src = Lockfile::default();
    let tgt = Lockfile::default();
    let mapping = Mapping::default();
    let mut subs = BTreeMap::new();
    subs.insert(
        "https://test/api/v1/hook_templates/39".to_string(),
        "https://prod/api/v1/hook_templates/41".to_string(),
    );

    let mut payload = serde_json::json!({
        "hook_template": "https://test/api/v1/hook_templates/39",
        "unrelated": "https://docs.rossum.ai"
    });
    rewrite_urls(&mut payload, &src, &tgt, &mapping, &subs);
    assert_eq!(payload["hook_template"].as_str().unwrap(), "https://prod/api/v1/hook_templates/41");
    assert_eq!(payload["unrelated"].as_str().unwrap(), "https://docs.rossum.ai");
}
```

(Update the four existing `rewrite_urls_*` tests in the same `mod tests` block to pass an empty `&BTreeMap::new()` as the new last arg — see Step 3.)

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p rdc --lib cli::deploy::common`
Expected: FAIL (signature mismatch on existing tests + new test).

- [ ] **Step 3: Update the function signature**

```rust
pub fn rewrite_urls(
    value: &mut Value,
    src_lockfile: &Lockfile,
    tgt_lockfile: &Lockfile,
    mapping: &Mapping,
    explicit_subs: &std::collections::BTreeMap<String, String>,
) {
    walk_strings_mut(value, &mut |s| {
        if let Some(tgt) = explicit_subs.get(s.as_str()) {
            *s = tgt.clone();
            return;
        }
        let Some((kind, src_slug)) = src_lockfile.lookup_url(s) else { return };
        let Some(tgt_slug) = mapping.lookup_tgt_slug(kind, src_slug) else { return };
        let Some(tgt_url) = tgt_lockfile.url_for_slug(kind, tgt_slug) else { return };
        *s = tgt_url.to_string();
    });
}
```

Then update every existing `rewrite_urls(...)` call site:

- `src/cli/deploy/apply.rs` — all 8 call sites (in hooks/rules/labels/queues/schemas/inboxes/email_templates/engines/engine_fields loops). For now pass `&BTreeMap::new()`. The hooks loop will get the real map in Task 16.
- `src/cli/deploy/create.rs::shape_create_body` — single call site. Pass `&BTreeMap::new()` (this will be threaded through in Task 19).

The four existing tests in `mod tests` need their `rewrite_urls(...)` calls updated to pass `&BTreeMap::new()` as the new last arg.

- [ ] **Step 4: Run all tests**

Run: `cargo test -p rdc`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/deploy/common.rs src/cli/deploy/apply.rs src/cli/deploy/create.rs
git commit -m "deploy: rewrite_urls takes explicit substitution map (for hook_template URLs)"
```

---

## Task 10: Effective `token_owner` resolver

**Files:**
- Create: `src/cli/deploy/store_extensions.rs`
- Modify: `src/cli/deploy/mod.rs`

This task adds the helper that decides which `token_owner` URL to use for a given store-extension slug, given the overlay (per-hook key, then `[defaults]` fallback, then `None`).

- [ ] **Step 1: Create `src/cli/deploy/store_extensions.rs` with the failing test**

```rust
//! Store-extension support for `rdc push` and `rdc deploy`. Centralises:
//!   - Effective `token_owner` resolution (per-hook overlay → defaults → None).
//!   - Template-URL resolution against the target cluster (Task 14).
//!   - Install-body construction (Task 12).
//!   - Interactive `token_owner` picker (Task 17).

use crate::overlay::Overlay;
use serde_json::Value;

/// Resolve the effective `token_owner` URL for a store extension on a
/// given environment. Order: per-hook overlay `token_owner` → overlay
/// `[defaults] store_extension_token_owner` → `None`.
pub fn effective_token_owner<'a>(overlay: Option<&'a Overlay>, slug: &str) -> Option<&'a str> {
    let overlay = overlay?;
    if let Some(per_hook) = overlay.hook(slug)
        .and_then(|m| m.get("token_owner"))
        .and_then(Value::as_str)
    {
        return Some(per_hook);
    }
    overlay.defaults.store_extension_token_owner.as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{Defaults, Overlay};
    use std::collections::BTreeMap;

    fn ov_with(per_hook: Option<&str>, default_url: Option<&str>) -> Overlay {
        let mut hooks = BTreeMap::new();
        if let Some(url) = per_hook {
            let mut entry = BTreeMap::new();
            entry.insert("token_owner".into(), Value::String(url.into()));
            hooks.insert("master-data-hub".into(), entry);
        }
        Overlay {
            version: 1,
            hooks,
            rules: BTreeMap::new(),
            labels: BTreeMap::new(),
            schemas: BTreeMap::new(),
            queues: BTreeMap::new(),
            inboxes: BTreeMap::new(),
            email_templates: BTreeMap::new(),
            engines: BTreeMap::new(),
            engine_fields: BTreeMap::new(),
            defaults: Defaults {
                store_extension_token_owner: default_url.map(|s| s.into()),
            },
        }
    }

    #[test]
    fn per_hook_wins_over_defaults() {
        let ov = ov_with(Some("https://per-hook"), Some("https://default"));
        assert_eq!(effective_token_owner(Some(&ov), "master-data-hub"), Some("https://per-hook"));
    }

    #[test]
    fn falls_back_to_defaults_when_no_per_hook() {
        let ov = ov_with(None, Some("https://default"));
        assert_eq!(effective_token_owner(Some(&ov), "master-data-hub"), Some("https://default"));
    }

    #[test]
    fn returns_none_when_neither_set() {
        let ov = ov_with(None, None);
        assert_eq!(effective_token_owner(Some(&ov), "master-data-hub"), None);
    }

    #[test]
    fn returns_none_when_no_overlay() {
        assert_eq!(effective_token_owner(None, "master-data-hub"), None);
    }
}
```

- [ ] **Step 2: Wire the module into `src/cli/deploy/mod.rs`**

Add: `pub mod store_extensions;`

- [ ] **Step 3: Run tests**

Run: `cargo test -p rdc --lib cli::deploy::store_extensions`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/cli/deploy/store_extensions.rs src/cli/deploy/mod.rs
git commit -m "deploy: effective_token_owner resolver (per-hook → defaults → None)"
```

---

## Task 11: Install-body builder

**Files:**
- Modify: `src/cli/deploy/store_extensions.rs`

The install endpoint accepts exactly `{name, hook_template, events, queues, token_owner}`. This helper extracts those five fields from a full hook body and returns them as a `serde_json::Value` ready to POST.

- [ ] **Step 1: Failing test in `store_extensions.rs::tests`**

```rust
#[test]
fn build_install_body_extracts_five_fields() {
    let full = serde_json::json!({
        "name": "Master Data Hub",
        "hook_template": "https://elis/api/v1/hook_templates/39",
        "events": ["annotation_content.initialize", "annotation_content.started"],
        "queues": ["https://elis/api/v1/queues/100", "https://elis/api/v1/queues/101"],
        "token_owner": "https://elis/api/v1/users/938493",
        "settings": { "configurations": ["customized"] },
        "active": false,
        "description": "must not appear in install body",
        "config": { "private": true }
    });
    let body = build_install_body(&full).unwrap();
    assert_eq!(body.as_object().unwrap().len(), 5);
    assert_eq!(body["name"].as_str().unwrap(), "Master Data Hub");
    assert_eq!(body["hook_template"].as_str().unwrap(), "https://elis/api/v1/hook_templates/39");
    assert_eq!(body["events"].as_array().unwrap().len(), 2);
    assert_eq!(body["queues"].as_array().unwrap().len(), 2);
    assert_eq!(body["token_owner"].as_str().unwrap(), "https://elis/api/v1/users/938493");
    assert!(body.get("settings").is_none());
    assert!(body.get("description").is_none());
}

#[test]
fn build_install_body_errors_when_required_field_missing() {
    let no_template = serde_json::json!({
        "name": "X", "events": [], "queues": [], "token_owner": "u"
    });
    assert!(build_install_body(&no_template).is_err());
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p rdc --lib cli::deploy::store_extensions::tests::build_install_body_`
Expected: FAIL (function doesn't exist).

- [ ] **Step 3: Implement in `store_extensions.rs`**

```rust
use anyhow::{anyhow, Result};

/// Extract `{name, hook_template, events, queues, token_owner}` from a
/// full hook body and return them as the `POST /hooks/create` payload.
/// Any field present but null counts as missing (matches the API).
pub fn build_install_body(full: &Value) -> Result<Value> {
    let obj = full.as_object()
        .ok_or_else(|| anyhow!("hook body is not a JSON object"))?;
    let mut out = serde_json::Map::new();
    for field in ["name", "hook_template", "events", "queues", "token_owner"] {
        let value = obj.get(field)
            .filter(|v| !v.is_null())
            .ok_or_else(|| anyhow!("store extension is missing required field '{field}' for /hooks/create"))?
            .clone();
        out.insert(field.to_string(), value);
    }
    Ok(Value::Object(out))
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rdc --lib cli::deploy::store_extensions`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/deploy/store_extensions.rs
git commit -m "deploy: build_install_body (5-field /hooks/create payload extractor)"
```

---

## Task 12: Orphan-check helper

**Files:**
- Modify: `src/cli/deploy/store_extensions.rs`

After an install (POST) succeeds but the follow-up PATCH fails, the next push/deploy must adopt the orphan rather than re-installing. The match criterion: same `(name, hook_template)` and no lockfile entry for that slug.

- [ ] **Step 1: Failing test**

```rust
#[test]
fn find_orphan_matches_by_name_and_template() {
    use crate::model::Hook;
    let hooks: Vec<Hook> = vec![
        serde_json::from_value(serde_json::json!({
            "id": 100, "url": "u100", "name": "Master Data Hub", "type": "webhook",
            "extension_source": "rossum_store",
            "hook_template": "https://elis/api/v1/hook_templates/39"
        })).unwrap(),
        serde_json::from_value(serde_json::json!({
            "id": 101, "url": "u101", "name": "Master Data Hub", "type": "webhook",
            "extension_source": "rossum_store",
            "hook_template": "https://elis/api/v1/hook_templates/27"  // different template
        })).unwrap(),
    ];
    let orphan = find_orphan(&hooks, "Master Data Hub", "https://elis/api/v1/hook_templates/39");
    assert_eq!(orphan.map(|h| h.id), Some(100));

    let none = find_orphan(&hooks, "No Such Hook", "https://elis/api/v1/hook_templates/39");
    assert!(none.is_none());
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p rdc --lib cli::deploy::store_extensions::tests::find_orphan_`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
use crate::model::Hook;

/// Find a remote hook matching `(name, hook_template)`. Used after a
/// previously-failed two-call create to adopt the partial install instead
/// of POSTing again.
pub fn find_orphan<'a>(hooks: &'a [Hook], name: &str, template_url: &str) -> Option<&'a Hook> {
    hooks.iter().find(|h| h.name == name && h.hook_template() == Some(template_url))
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rdc --lib cli::deploy::store_extensions::tests::find_orphan_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/deploy/store_extensions.rs
git commit -m "deploy: find_orphan helper for (name, hook_template) lookup"
```

---

## Task 13: Anomaly guard — store extension without `hook_template`

**Files:**
- Modify: `src/cli/deploy/store_extensions.rs`

- [ ] **Step 1: Failing test**

```rust
#[test]
fn check_anomaly_passes_for_regular_hook() {
    let payload = serde_json::json!({"name": "x", "extension_source": "custom"});
    let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
    assert!(check_store_extension_anomaly(&hook, "x").is_ok());
}

#[test]
fn check_anomaly_passes_for_store_extension_with_template() {
    let payload = serde_json::json!({
        "name": "x", "extension_source": "rossum_store",
        "hook_template": "https://x/api/v1/hook_templates/1"
    });
    let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
    assert!(check_store_extension_anomaly(&hook, "x").is_ok());
}

#[test]
fn check_anomaly_rejects_store_extension_without_template() {
    let payload = serde_json::json!({
        "name": "x", "extension_source": "rossum_store"
    });
    let hook: crate::model::Hook = serde_json::from_value(payload).unwrap();
    let err = check_store_extension_anomaly(&hook, "broken-slug").unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("broken-slug"), "error should name the slug: {msg}");
    assert!(msg.contains("hook_template"), "error should explain the problem: {msg}");
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p rdc --lib cli::deploy::store_extensions::tests::check_anomaly_`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
/// Defensive guard: a hook with `extension_source: "rossum_store"` must
/// always have `hook_template` set. Production data should never violate
/// this, but a hand-edited snapshot could.
pub fn check_store_extension_anomaly(hook: &Hook, slug: &str) -> Result<()> {
    if hook.is_store_extension() && hook.hook_template().is_none() {
        return Err(anyhow!(
            "hooks/{slug}.json: marked as store extension (extension_source = rossum_store) but missing hook_template URL — refusing to push"
        ));
    }
    Ok(())
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p rdc --lib cli::deploy::store_extensions`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/deploy/store_extensions.rs
git commit -m "deploy: check_store_extension_anomaly guard"
```

---

## Task 14: Template-URL index builder

**Files:**
- Modify: `src/cli/deploy/store_extensions.rs`

Build a `BTreeMap<src_template_url, tgt_template_url>` by joining src-side templates (fetched once from src cluster) and tgt-side templates (fetched once from tgt cluster) on `(name, type, extension_source)`. Returns an error if a needed template has no match or ambiguous matches on tgt.

- [ ] **Step 1: Failing test**

```rust
#[test]
fn build_template_url_map_pairs_by_name_type_source() {
    use crate::model::HookTemplate;
    let src: Vec<HookTemplate> = serde_json::from_value(serde_json::json!([
        {"url": "https://test/api/v1/hook_templates/39", "name": "Master Data Hub",
         "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"},
        {"url": "https://test/api/v1/hook_templates/27", "name": "Email Notifications",
         "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
    ])).unwrap();
    let tgt: Vec<HookTemplate> = serde_json::from_value(serde_json::json!([
        {"url": "https://prod/api/v1/hook_templates/41", "name": "Master Data Hub",
         "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"},
        {"url": "https://prod/api/v1/hook_templates/27", "name": "Email Notifications",
         "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
    ])).unwrap();
    let needed = ["https://test/api/v1/hook_templates/39",
                  "https://test/api/v1/hook_templates/27"];
    let map = build_template_url_map(&needed, &src, &tgt, "prod").unwrap();
    assert_eq!(map["https://test/api/v1/hook_templates/39"],
               "https://prod/api/v1/hook_templates/41");
    assert_eq!(map["https://test/api/v1/hook_templates/27"],
               "https://prod/api/v1/hook_templates/27");
}

#[test]
fn build_template_url_map_errors_on_missing_tgt() {
    use crate::model::HookTemplate;
    let src: Vec<HookTemplate> = serde_json::from_value(serde_json::json!([
        {"url": "https://test/api/v1/hook_templates/39", "name": "Master Data Hub",
         "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
    ])).unwrap();
    let tgt: Vec<HookTemplate> = vec![];
    let err = build_template_url_map(&["https://test/api/v1/hook_templates/39"], &src, &tgt, "prod").unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("Master Data Hub"));
    assert!(msg.contains("not available on prod"));
}

#[test]
fn build_template_url_map_errors_on_ambiguous_tgt() {
    use crate::model::HookTemplate;
    let src: Vec<HookTemplate> = serde_json::from_value(serde_json::json!([
        {"url": "https://test/api/v1/hook_templates/39", "name": "MDH",
         "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
    ])).unwrap();
    let tgt: Vec<HookTemplate> = serde_json::from_value(serde_json::json!([
        {"url": "https://prod/api/v1/hook_templates/41", "name": "MDH",
         "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"},
        {"url": "https://prod/api/v1/hook_templates/42", "name": "MDH",
         "type": "webhook", "extension_source": "rossum_store", "install_action": "copy"}
    ])).unwrap();
    let err = build_template_url_map(&["https://test/api/v1/hook_templates/39"], &src, &tgt, "prod").unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("ambiguous"));
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p rdc --lib cli::deploy::store_extensions::tests::build_template_url_map_`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
use crate::model::HookTemplate;
use std::collections::BTreeMap;

/// Build a `src_template_url → tgt_template_url` map by matching templates
/// on `(name, type, extension_source)`. Only templates appearing in
/// `needed_src_urls` are looked up — irrelevant templates are skipped to
/// keep the error surface focused.
pub fn build_template_url_map(
    needed_src_urls: &[&str],
    src_templates: &[HookTemplate],
    tgt_templates: &[HookTemplate],
    tgt_env_label: &str,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for src_url in needed_src_urls {
        let src = src_templates.iter().find(|t| t.url == *src_url)
            .ok_or_else(|| anyhow!(
                "internal: needed src template '{src_url}' not present in src template listing — pull the src env first"
            ))?;
        let key = (src.name.as_str(), src.template_type.as_str(), src.extension_source.as_str());
        let matches: Vec<&HookTemplate> = tgt_templates.iter()
            .filter(|t| (t.name.as_str(), t.template_type.as_str(), t.extension_source.as_str()) == key)
            .collect();
        match matches.len() {
            0 => return Err(anyhow!(
                "template '{}' is not available on {tgt_env_label}. Templates with install_action=request_access require Rossum sales to enable; copy templates may have been withdrawn. Install manually via the UI on {tgt_env_label}, then re-run rdc pull {tgt_env_label}.",
                src.name
            )),
            1 => { out.insert(src_url.to_string(), matches[0].url.clone()); }
            n => {
                let ids: Vec<&str> = matches.iter()
                    .map(|t| t.url.rsplit('/').next().unwrap_or("?"))
                    .collect();
                return Err(anyhow!(
                    "ambiguous templates for '{}' on {tgt_env_label} ({n} matches, ids {}); add a mapping under [hook_templates] in .rdc/map/<src>-to-{tgt_env_label}.toml.",
                    src.name,
                    ids.join(", ")
                ));
            }
        }
    }
    Ok(out)
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rdc --lib cli::deploy::store_extensions`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/deploy/store_extensions.rs
git commit -m "deploy: build_template_url_map by (name, type, extension_source) match"
```

---

## Task 15: Push — store-extension dispatch + two-call create

**Files:**
- Modify: `src/cli/push/hooks.rs`

This is the central same-env push change. When the local hook is a store extension and the lockfile entry is missing, branch into a three-phase path (orphan check → install → PATCH).

- [ ] **Step 1: Add a shared test-fixture helper at the top of `tests/cli_push.rs`**

(Tests in this plan reuse the MDH-shaped body; define it once.)

```rust
fn mdh_snapshot_body(api_base: &str) -> serde_json::Value {
    serde_json::json!({
        "name": "Master Data Hub",
        "type": "webhook",
        "events": ["annotation_content.initialize", "annotation_content.started", "annotation_content.updated"],
        "queues": [],
        "active": true,
        "run_after": [],
        "metadata": {},
        "config": { "private": true, "timeout_s": 60, "payload_logging_enabled": false },
        "settings": { "configurations": [{"name": "customised"}] },
        "sideload": ["schemas"],
        "settings_schema": null,
        "secrets_schema": { "type": "object", "additionalProperties": {"type": "string"} },
        "description": "Enhance the extracted data with details from your master records.",
        "guide": "<div>...</div>",
        "read_more_url": "https://docs.rossum.ai/mdh",
        "extension_image_url": "https://example.com/mdh.png",
        "token_lifetime_s": 7200,
        "token_owner": format!("{api_base}/users/938493"),
        "extension_source": "rossum_store",
        "hook_template": format!("{api_base}/hook_templates/39")
    })
}

/// Same shape as `mdh_snapshot_body` but representing what the server
/// returns immediately after `POST /hooks/create` — template defaults
/// (un-customised `settings`), plus id/url assigned.
fn mdh_installed_body(api_base: &str, id: u64) -> serde_json::Value {
    let mut body = mdh_snapshot_body(api_base);
    body["id"] = serde_json::Value::from(id);
    body["url"] = serde_json::Value::from(format!("{api_base}/hooks/{id}"));
    body["settings"] = serde_json::json!({"configurations": []});  // template default, NOT customised
    body
}
```

- [ ] **Step 2: Failing wiremock integration test in `tests/cli_push.rs`**

(Follow the existing test pattern in `tests/cli_push.rs` — set up a project dir with `rdc.toml` + `secrets/test.secrets.json`, write `envs/test/hooks/master-data-hub.json` without a lockfile entry, mock the API responses, run `rdc push test --yes`, assert the install + PATCH calls fired and the lockfile got the new id.)

```rust
#[tokio::test]
async fn push_installs_store_extension_via_two_call_flow() {
    let project = TestProject::new();
    let server = MockServer::start().await;
    project.write_rdc_toml(&[("test", &server.uri())]);
    project.write_token("test", "TKN");

    // Snapshot has the hook, no lockfile entry.
    project.write_hook_file("test", "master-data-hub", &mdh_snapshot_body(&server.uri()));

    // 1. Orphan check — empty list.
    Mock::given(method("GET")).and(path("/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({
            "pagination": {"next": null}, "results": []
        })))
        .expect(1)
        .mount(&server).await;

    // 2. Install — POST /hooks/create.
    Mock::given(method("POST")).and(path("/hooks/create"))
        .respond_with(ResponseTemplate::new(201).set_body_json(&mdh_installed_body(&server.uri(), 999)))
        .expect(1)
        .mount(&server).await;

    // 3. PATCH — customisation reconcile.
    let after_patch = mdh_snapshot_body(&server.uri()); // includes the customised settings
    Mock::given(method("PATCH")).and(path("/hooks/999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&{
            let mut b = after_patch;
            b["id"] = serde_json::Value::from(999);
            b["url"] = serde_json::Value::from(format!("{}/hooks/999", server.uri()));
            b
        }))
        .expect(1)
        .mount(&server).await;

    project.run(&["push", "test", "--yes"]).assert().success();

    let lf = project.load_lockfile("test");
    assert_eq!(lf.objects["hooks"]["master-data-hub"].id, 999);
}
```

(If `TestProject` doesn't already exist as a helper, add it next to the existing tests' setup boilerplate — look at how `tests/cli_pull.rs` or `tests/cli_push.rs` mints `rdc.toml` + secret + lockfile + snapshot files today.)

- [ ] **Step 3: Run the test, expect fail**

Run: `cargo test --test cli_push push_installs_store_extension_via_two_call_flow`
Expected: FAIL — push currently posts to `/hooks/` which the mock doesn't expect.

- [ ] **Step 4: Implement the dispatch in `src/cli/push/hooks.rs`**

Replace the "missing lockfile entry → POST /hooks/" branch:

```rust
if lockfile.objects.get("hooks").and_then(|m| m.get(slug.as_str())).is_none() {
    // Read + overlay-apply once; reused by both paths.
    let mut payload = read_hook_value(&hooks_dir, slug)
        .with_context(|| format!("reading local hook '{slug}' for create"))?;
    if let Some(p) = overlay_paths {
        apply_overrides(&mut payload, p);
    }

    // Anomaly guard, then dispatch.
    let typed: crate::model::Hook = serde_json::from_value(payload.clone())
        .with_context(|| format!("deserializing hook '{slug}' for create"))?;
    crate::cli::deploy::store_extensions::check_store_extension_anomaly(&typed, slug)?;

    let created = if typed.is_store_extension() {
        // Two-call create.
        if remote_hooks.is_none() {
            remote_hooks = Some(client.list_hooks(Some(progress.clone())).await
                .context("listing hooks for store-extension orphan check")?);
        }
        let remote = remote_hooks.as_ref().unwrap();
        let template_url = typed.hook_template().unwrap(); // anomaly guard ensured it's Some
        let installed_id = match crate::cli::deploy::store_extensions::find_orphan(remote, &typed.name, template_url) {
            Some(orphan) => {
                progress.println(format!("adopting orphan store-extension hooks/{slug} (id {})", orphan.id));
                orphan.id
            }
            None => {
                let install_body = crate::cli::deploy::store_extensions::build_install_body(&payload)?;
                let installed = client.create_hook_via_install(&install_body, Some(progress.clone())).await
                    .with_context(|| format!("POST /hooks/create (installing store extension '{slug}')"))?;
                progress.println(format!("installed store extension hooks/{slug} (id {})", installed.id));
                installed.id
            }
        };
        client.update_hook(installed_id, &typed, Some(progress.clone())).await
            .with_context(|| format!("PATCH /hooks/{installed_id} (reconciling store extension '{slug}')"))?
    } else {
        // Existing regular-hook POST path.
        strip_for_create(&mut payload, "hooks");
        client.create_hook(&payload, Some(progress.clone())).await
            .with_context(|| format!("POST /hooks (creating '{slug}')"))?
    };

    // Disk + lockfile write — unchanged from the existing single-call path.
    let (created_json_full, created_code) = serialize_hook(&created)?;
    let created_json_stripped = maybe_strip_overlay(created_json_full, overlay_paths)?;
    let created_hash = hook_combined_hash(&created_json_stripped, &created_code);
    write_atomic(local_json_path, &created_json_stripped)
        .with_context(|| format!("writing post-create canonical form for '{slug}'"))?;
    if let Some(code) = &created_code {
        write_hook_code(&hooks_dir, slug, code)
            .with_context(|| format!("writing hook code for '{slug}'"))?;
    }
    lockfile.upsert("hooks", slug, ObjectEntry {
        id: created.id,
        url: Some(created.url.clone()),
        modified_at: created.modified_at().map(|s| s.to_string()),
        content_hash: Some(created_hash),
    });
    progress.println(format!("created hooks/{slug} (id {})", created.id));
    progress.tick(slug.as_str());
    pushed += 1;
    continue;
}
```

(Note `remote_hooks` is already a cached `Option<Vec<Hook>>` from the existing code below this branch — the orphan check reuses it.)

- [ ] **Step 5: Run the integration test, expect pass**

Run: `cargo test --test cli_push push_installs_store_extension_via_two_call_flow`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cli/push/hooks.rs tests/cli_push.rs
git commit -m "push: two-call install path for store extensions"
```

---

## Task 16: Push orphan-recovery test

**Files:**
- Modify: `tests/cli_push.rs`

Cover the case where a prior push died between install and PATCH: the next push finds the orphan, skips the install, runs PATCH only.

- [ ] **Step 1: Add the failing test**

```rust
#[tokio::test]
async fn push_adopts_orphan_store_extension_skips_install() {
    let project = TestProject::new();
    let server = MockServer::start().await;
    project.write_rdc_toml(&[("test", &server.uri())]);
    project.write_token("test", "TKN");

    project.write_hook_file("test", "master-data-hub", &mdh_snapshot_body(&server.uri()));

    // GET /hooks returns the orphan from a previous failed PATCH.
    let orphan = mdh_installed_body(&server.uri(), 555);
    Mock::given(method("GET")).and(path("/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
            "pagination": {"next": null}, "results": [orphan.clone()]
        })))
        .expect(1)
        .mount(&server).await;

    // POST must NOT happen — verify by mounting a 500 fallback the test fails on.
    Mock::given(method("POST")).and(path("/hooks/create"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server).await;

    // PATCH is the only write call.
    Mock::given(method("PATCH")).and(path("/hooks/555"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&orphan))
        .expect(1)
        .mount(&server).await;

    project.run(&["push", "test", "--yes"]).assert().success();
    let lf = project.load_lockfile("test");
    assert_eq!(lf.objects["hooks"]["master-data-hub"].id, 555);
}
```

- [ ] **Step 2: Run, expect pass**

Run: `cargo test --test cli_push push_adopts_orphan_store_extension_skips_install`
Expected: PASS (the dispatch from Task 15 already handles this).

- [ ] **Step 3: Commit**

```bash
git add tests/cli_push.rs
git commit -m "push: regression test for orphan store-extension adoption"
```

---

## Task 17: Interactive `token_owner` picker

**Files:**
- Modify: `src/cli/resolve.rs`
- Modify: `src/cli/deploy/store_extensions.rs` (re-export the picker for deploy)

- [ ] **Step 1: Failing test in `src/cli/resolve.rs::tests`**

```rust
#[test]
fn picker_renders_users_in_priority_order() {
    use crate::model::User;
    let users: Vec<User> = serde_json::from_value(serde_json::json!([
        {"id": 100, "url": "u100", "username": "alice@x", "first_name": "Alice", "last_name": "",
         "is_active": true, "groups": ["https://x/groups/3"]},
        {"id": 938493, "url": "u938493", "username": "system_user__abc", "first_name": "SYS",
         "last_name": "USER", "is_active": true, "groups": ["https://x/groups/3"]},
        {"id": 200, "url": "u200", "username": "bob@x", "first_name": "Bob", "last_name": "",
         "is_active": true, "groups": ["https://x/groups/3"]}
    ])).unwrap();
    let rendered = render_token_owner_picker("master-data-hub", "prod", &users, 938493);
    let lines: Vec<&str> = rendered.lines().collect();
    // System user first.
    let first_url_line = lines.iter().find(|l| l.contains("u938493")).unwrap();
    let alice_url_line = lines.iter().find(|l| l.contains("u100")).unwrap();
    assert!(rendered.find(first_url_line) < rendered.find(alice_url_line),
            "system_user__ should be ranked first");
    // Active session's own user tagged.
    assert!(rendered.contains("you"));
}
```

(Add a test in the existing `tests` block for resolve.rs.)

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p rdc --lib cli::resolve::tests::picker_renders_users_in_priority_order`
Expected: FAIL.

- [ ] **Step 3: Implement the render + the interactive variant**

```rust
// Pure render function (testable without TTY).
pub fn render_token_owner_picker(slug: &str, tgt_env: &str, users: &[crate::model::User], self_id: u64) -> String {
    let mut sorted: Vec<&crate::model::User> = users.iter().collect();
    sorted.sort_by_key(|u| {
        // Sort key: 0 = system_user__, 1 = other admins, 2 = the rest. Stable within.
        if u.is_system_user() { 0 } else if u.is_admin() { 1 } else { 2 }
    });
    let mut out = String::new();
    out.push_str(&format!("Pick the token_owner for store extension '{slug}' on {tgt_env}\n"));
    out.push_str("(used as the API service account for the extension's calls; usually a system user):\n\n");
    for (i, u) in sorted.iter().enumerate() {
        let tags = {
            let mut t = Vec::new();
            if u.is_admin() { t.push("admin"); }
            if u.is_active { t.push("active"); }
            if u.id == self_id { t.push("you"); }
            t.join(", ")
        };
        let display = if u.first_name.is_empty() && u.last_name.is_empty() {
            u.username.clone()
        } else {
            format!("{} {}", u.first_name, u.last_name).trim().to_string()
        };
        out.push_str(&format!("  [{}] {display}   {tags}\n", i + 1));
        out.push_str(&format!("      {}\n", u.url));
    }
    out.push_str("  [a] abort the deploy\n\n");
    out.push_str("[1] > ");
    out
}

/// Prompt interactively. Returns `(picked_user_url, apply_to_all)` or
/// `None` if the user aborted. Non-TTY callers must skip this and check
/// the overlay state up-front (see Task 19).
pub fn prompt_token_owner(slug: &str, tgt_env: &str, users: &[crate::model::User], self_id: u64) -> Result<Option<(String, bool)>> {
    use std::io::{self, BufRead, Write};
    let rendered = render_token_owner_picker(slug, tgt_env, users, self_id);
    print!("{rendered}");
    io::stdout().flush().ok();
    let stdin = io::stdin();
    let line = stdin.lock().lines().next().ok_or_else(|| anyhow::anyhow!("stdin closed"))??;
    let pick = line.trim();
    if pick == "a" { return Ok(None); }
    let n: usize = if pick.is_empty() { 1 } else { pick.parse().map_err(|_| anyhow::anyhow!("expected a number or 'a', got '{pick}'"))? };
    let mut sorted: Vec<&crate::model::User> = users.iter().collect();
    sorted.sort_by_key(|u| if u.is_system_user() { 0 } else if u.is_admin() { 1 } else { 2 });
    let chosen = sorted.get(n - 1).ok_or_else(|| anyhow::anyhow!("'{n}' is out of range"))?;
    print!("\nApply this choice to all remaining store extensions in this deploy? [y/N] ");
    io::stdout().flush().ok();
    let line2 = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
    let apply_all = matches!(line2.trim().to_lowercase().as_str(), "y" | "yes");
    Ok(Some((chosen.url.clone(), apply_all)))
}
```

- [ ] **Step 4: Re-export from `store_extensions.rs`**

```rust
pub use crate::cli::resolve::{prompt_token_owner, render_token_owner_picker};
```

- [ ] **Step 5: Run tests, expect pass**

Run: `cargo test -p rdc --lib cli::resolve::tests::picker_renders_users_in_priority_order`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cli/resolve.rs src/cli/deploy/store_extensions.rs
git commit -m "resolve: interactive token_owner picker (rank system_user__* first)"
```

---

## Task 18: Overlay write helper

**Files:**
- Modify: `src/overlay.rs`

A small helper that writes a per-hook `token_owner` (or the `[defaults]` value) back to disk. Must preserve unrelated overlay entries and pretty-format the TOML.

- [ ] **Step 1: Failing test in `src/overlay.rs::tests`**

```rust
#[test]
fn write_token_owner_creates_per_hook_entry() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("overlay.toml");
    std::fs::write(&path, "version = 1\n\n[hooks.other-hook]\n\"name\" = \"Other\"\n").unwrap();

    write_store_extension_token_owner(&path, Some("master-data-hub"), "https://prod/api/v1/users/938493").unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("[hooks.master-data-hub]"));
    assert!(raw.contains("token_owner = \"https://prod/api/v1/users/938493\""));
    assert!(raw.contains("[hooks.other-hook]"), "existing entries must be preserved");
    assert!(raw.contains("Other"));
}

#[test]
fn write_token_owner_creates_defaults_entry_when_slug_none() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("overlay.toml");
    std::fs::write(&path, "version = 1\n").unwrap();

    write_store_extension_token_owner(&path, None, "https://prod/api/v1/users/938493").unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("[defaults]"));
    assert!(raw.contains("store_extension_token_owner = \"https://prod/api/v1/users/938493\""));
}

#[test]
fn write_token_owner_creates_file_if_missing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("overlay.toml");
    // file doesn't exist
    write_store_extension_token_owner(&path, None, "https://prod/api/v1/users/938493").unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("version = 1"));
    assert!(raw.contains("[defaults]"));
    assert!(raw.contains("store_extension_token_owner"));
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test -p rdc --lib overlay::tests::write_token_owner_`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
/// Idempotent write: load the overlay file (or create an empty one),
/// patch in the `token_owner` (per-hook if `slug` is `Some`, otherwise
/// into `[defaults] store_extension_token_owner`), atomically rewrite
/// the TOML. Preserves every other key.
pub fn write_store_extension_token_owner(
    path: &Path,
    slug: Option<&str>,
    user_url: &str,
) -> Result<()> {
    let mut overlay = Overlay::load(path)?.unwrap_or(Overlay {
        version: 1,
        hooks: BTreeMap::new(),
        rules: BTreeMap::new(),
        labels: BTreeMap::new(),
        schemas: BTreeMap::new(),
        queues: BTreeMap::new(),
        inboxes: BTreeMap::new(),
        email_templates: BTreeMap::new(),
        engines: BTreeMap::new(),
        engine_fields: BTreeMap::new(),
        defaults: Defaults::default(),
    });
    match slug {
        Some(s) => {
            let entry = overlay.hooks.entry(s.to_string()).or_insert_with(BTreeMap::new);
            entry.insert("token_owner".into(), Value::String(user_url.into()));
        }
        None => {
            overlay.defaults.store_extension_token_owner = Some(user_url.into());
        }
    }
    let s = toml::to_string_pretty(&overlay).context("serializing overlay")?;
    crate::snapshot::writer::write_atomic(path, s.as_bytes())?;
    Ok(())
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test -p rdc --lib overlay`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/overlay.rs
git commit -m "overlay: write_store_extension_token_owner helper (idempotent patch-in)"
```

---

## Task 19: Deploy pre-pass — resolve templates + prompt for token_owner

**Files:**
- Modify: `src/cli/deploy/store_extensions.rs`
- Modify: `src/cli/deploy/run.rs` (or wherever the deploy entry point currently runs `apply::run` / `create::*`)

This wires the building blocks together: at the start of a deploy, before any POST/PATCH, walk the src snapshot for store-extension hooks that need bootstrap, resolve their template URLs against tgt, and prompt for any missing `token_owner`. Persist results to the map cache and the tgt overlay.

- [ ] **Step 1: Failing wiremock test for `tests/cli_deploy.rs`**

```rust
#[tokio::test]
async fn deploy_resolves_templates_and_prompts_for_token_owner() {
    let project = TestProject::new();
    let src = MockServer::start().await;
    let tgt = MockServer::start().await;
    project.write_rdc_toml(&[("test", &src.uri()), ("prod", &tgt.uri())]);
    project.write_token("test", "TKN_TEST");
    project.write_token("prod", "TKN_PROD");

    // Pull state: src has a store extension (MDH), tgt has no hooks yet.
    // (Reuses mdh_snapshot_body from cli_push.rs; copy it into cli_deploy.rs
    // — or pull it into a shared `tests/common/mod.rs` if you prefer.)
    project.write_hook_file("test", "master-data-hub", &mdh_snapshot_body(&src.uri()));
    project.write_lockfile_entry("test", "hooks", "master-data-hub", 999, &format!("{}/hooks/999", src.uri()));
    project.write_pull_state("prod", &[]); // empty

    // Tgt cluster template list — same name, different id.
    Mock::given(method("GET")).and(path("/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
            "pagination": {"next": null},
            "results": [
                {"url": format!("{}/hook_templates/41", tgt.uri()),
                 "name": "Master Data Hub", "type": "webhook",
                 "extension_source": "rossum_store", "install_action": "copy"}
            ]
        })))
        .mount(&tgt).await;
    // Src cluster template list — needed for `(name, type, source)` lookup of the src URL.
    Mock::given(method("GET")).and(path("/hook_templates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
            "pagination": {"next": null},
            "results": [
                {"url": format!("{}/hook_templates/39", src.uri()),
                 "name": "Master Data Hub", "type": "webhook",
                 "extension_source": "rossum_store", "install_action": "copy"}
            ]
        })))
        .mount(&src).await;
    // Tgt users — one system user + one admin.
    Mock::given(method("GET")).and(path("/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&json!({
            "pagination": {"next": null},
            "results": [
                {"id": 521884, "url": format!("{}/users/521884", tgt.uri()),
                 "username": "system_user__prod", "is_active": true,
                 "groups": [format!("{}/groups/3", tgt.uri())]}
            ]
        })))
        .mount(&tgt).await;
    // Install + PATCH on tgt. The install body's hook_template will
    // already be the tgt URL (resolved by build_template_url_map); fix the
    // response template URL accordingly so it survives the round-trip.
    let installed = {
        let mut b = mdh_installed_body(&tgt.uri(), 700);
        b["hook_template"] = serde_json::Value::String(format!("{}/hook_templates/41", tgt.uri()));
        b
    };
    Mock::given(method("POST")).and(path("/hooks/create"))
        .respond_with(ResponseTemplate::new(201).set_body_json(&installed))
        .mount(&tgt).await;
    let customised = {
        let mut b = mdh_snapshot_body(&tgt.uri());
        b["id"] = serde_json::Value::from(700);
        b["url"] = serde_json::Value::from(format!("{}/hooks/700", tgt.uri()));
        b["hook_template"] = serde_json::Value::String(format!("{}/hook_templates/41", tgt.uri()));
        b
    };
    Mock::given(method("PATCH")).and(path_regex(r"^/hooks/\d+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&customised))
        .mount(&tgt).await;

    // Non-TTY path: pre-populate the overlay with the system user so deploy doesn't hang on stdin.
    project.write_overlay("prod", r#"
version = 1
[defaults]
store_extension_token_owner = "TGT_USER_URL"
"#.replace("TGT_USER_URL", &format!("{}/users/521884", tgt.uri())).as_str());

    project.run(&["deploy", "test", "prod", "--yes"]).assert().success();

    // The map cache should now contain the template pair.
    let map_path = project.dot_rdc_dir().join("map").join("test-to-prod.toml");
    let raw = std::fs::read_to_string(&map_path).unwrap();
    assert!(raw.contains("[hook_templates]"));
    assert!(raw.contains("hook_templates/41"));
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test --test cli_deploy deploy_resolves_templates_and_prompts_for_token_owner`
Expected: FAIL.

- [ ] **Step 3: Implement the pre-pass in `store_extensions.rs`**

```rust
use crate::api::RossumClient;
use crate::mapping::Mapping;
use crate::overlay::{write_store_extension_token_owner, Overlay};
use crate::state::Lockfile;
use crate::progress::ProgressHandle;
use std::path::Path;

/// One bootstrap entry per store extension to be created.
#[derive(Debug, Clone)]
pub struct StorePlan {
    pub src_slug: String,
    pub tgt_slug: String,
    pub src_template_url: String,
    pub tgt_template_url: String,
    pub tgt_template_name: String,
    pub token_owner_url: String,
}

/// Build the bootstrap list. Side-effects:
///   - lists src + tgt `/hook_templates` (only when the in-memory map cache misses)
///   - lists tgt `/users` only if at least one missing `token_owner`
///   - prompts (TTY) or refuses (non-TTY) for missing `token_owner` values
///   - writes the chosen URLs to the tgt overlay file
///   - inserts resolved template URL pairs into `mapping.hook_templates`
///     (the caller is responsible for persisting the mapping)
pub async fn plan_store_extension_bootstrap(
    src_paths: &crate::paths::Paths,
    tgt_paths: &crate::paths::Paths,
    src_client: &RossumClient,
    tgt_client: &RossumClient,
    _src_lockfile: &Lockfile,
    tgt_lockfile: &Lockfile,
    mapping: &mut Mapping,
    tgt_overlay_path: &Path,
    interactive: bool,
    self_user_id: u64,
    tgt_env_label: &str,
    progress: ProgressHandle,
) -> Result<Vec<StorePlan>> {
    // 1. Walk src snapshot hooks/, identify store-extension hooks that need
    //    bootstrap (no tgt lockfile entry).
    let hooks_dir = src_paths.hooks_dir();
    let mut needed: Vec<(String, crate::model::Hook)> = Vec::new();
    if hooks_dir.exists() {
        for entry in std::fs::read_dir(&hooks_dir)
            .with_context(|| format!("reading {}", hooks_dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let slug = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            let hook = crate::snapshot::hook::read_hook(&hooks_dir, &slug)?;
            if !hook.is_store_extension() {
                continue;
            }
            check_store_extension_anomaly(&hook, &slug)?;
            // Auto-mapping: same slug on tgt by default.
            let tgt_slug = mapping.lookup_tgt_slug("hooks", &slug)
                .map(|s| s.to_string())
                .unwrap_or_else(|| slug.clone());
            // Skip if tgt already has it (this is an update path, not bootstrap).
            if tgt_lockfile.objects.get("hooks").and_then(|m| m.get(&tgt_slug)).is_some() {
                continue;
            }
            needed.push((tgt_slug, hook));
        }
    }
    if needed.is_empty() {
        return Ok(Vec::new());
    }

    // 2. Resolve template URLs. List on cache miss only.
    let src_urls: std::collections::BTreeSet<String> = needed.iter()
        .filter_map(|(_, h)| h.hook_template().map(|s| s.to_string()))
        .collect();
    let uncached: Vec<&str> = src_urls.iter()
        .filter(|u| !mapping.hook_templates.contains_key(*u))
        .map(|s| s.as_str())
        .collect();
    let mut tgt_templates: Vec<crate::model::HookTemplate> = Vec::new();
    if !uncached.is_empty() {
        let src_templates = src_client.list_hook_templates(progress.clone()).await
            .context("listing src hook templates")?;
        tgt_templates = tgt_client.list_hook_templates(progress.clone()).await
            .context("listing tgt hook templates")?;
        let pairs = build_template_url_map(&uncached, &src_templates, &tgt_templates, tgt_env_label)?;
        mapping.hook_templates.extend(pairs);
    } else {
        // Even with cache hits, we need tgt_templates for plan-output names.
        tgt_templates = tgt_client.list_hook_templates(progress.clone()).await
            .context("listing tgt hook templates for plan output")?;
    }

    // 3. Resolve token_owner per hook. Prompt at most once if user picks
    //    "apply to all"; that fills `default_url`, which short-circuits
    //    subsequent prompts in this run.
    let mut overlay = Overlay::load(tgt_overlay_path)?;
    let mut tgt_users: Option<Vec<crate::model::User>> = None;
    let mut default_url: Option<String> = overlay.as_ref()
        .and_then(|ov| ov.defaults.store_extension_token_owner.clone());
    let mut plans = Vec::new();
    for (tgt_slug, hook) in &needed {
        let src_slug = tgt_slug.clone(); // same-slug default; rename via mapping handled at step 1
        let src_template_url = hook.hook_template().unwrap().to_string();
        let tgt_template_url = mapping.hook_templates.get(&src_template_url)
            .ok_or_else(|| anyhow!("internal: template URL '{src_template_url}' missing from mapping after resolve"))?
            .clone();
        let tgt_template_name = tgt_templates.iter().find(|t| t.url == tgt_template_url)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| "<unknown>".into());

        let resolved = effective_token_owner(overlay.as_ref(), tgt_slug)
            .map(|s| s.to_string())
            .or_else(|| default_url.clone());

        let token_owner_url = match resolved {
            Some(u) => u,
            None => {
                if !interactive {
                    return Err(anyhow!(
                        "deploy needs token_owner for store extension '{tgt_slug}' on {tgt_env_label}, but {} has no [hooks.{tgt_slug}] token_owner and no [defaults] store_extension_token_owner.\nRun 'rdc deploy <src> {tgt_env_label}' on a TTY once to pick interactively, or edit the overlay directly. Aborting before any remote writes.",
                        tgt_overlay_path.display()
                    ));
                }
                if tgt_users.is_none() {
                    tgt_users = Some(tgt_client.list_users(progress.clone()).await
                        .context("listing tgt users for token_owner picker")?);
                }
                let users = tgt_users.as_ref().unwrap();
                let (chosen, apply_all) = match prompt_token_owner(tgt_slug, tgt_env_label, users, self_user_id)? {
                    Some(pair) => pair,
                    None => return Err(anyhow!("deploy aborted at token_owner picker")),
                };
                if apply_all {
                    write_store_extension_token_owner(tgt_overlay_path, None, &chosen)?;
                    default_url = Some(chosen.clone());
                } else {
                    write_store_extension_token_owner(tgt_overlay_path, Some(tgt_slug), &chosen)?;
                }
                // Reload overlay so subsequent lookups see the write.
                overlay = Overlay::load(tgt_overlay_path)?;
                chosen
            }
        };

        plans.push(StorePlan {
            src_slug,
            tgt_slug: tgt_slug.clone(),
            src_template_url,
            tgt_template_url,
            tgt_template_name,
            token_owner_url,
        });
    }
    Ok(plans)
}
```

- [ ] **Step 4: Call the pre-pass from the deploy entry point**

In `src/cli/deploy/run.rs` (or wherever `apply::run` is currently invoked), insert before the bootstrap loop. The picker tags the active session's user when `self_user_id` is `Some` — pass `None` for now (a follow-up can wire `/users/me` later):

```rust
let interactive = !args.yes && atty::is(atty::Stream::Stdin);
let store_plans = crate::cli::deploy::store_extensions::plan_store_extension_bootstrap(
    &src_paths, &tgt_paths,
    &src_client, &tgt_client,
    &src_lockfile, &tgt_lockfile,
    &mut mapping,
    &tgt_paths.overlay_file(),
    interactive,
    None,  // self user id — picker just won't tag "you" in the list
    tgt,
    None,
).await?;
mapping.save(&src_paths.mapping_file(src, tgt))?;
```

(Update the `plan_store_extension_bootstrap` signature in Step 3 so `self_user_id` is `Option<u64>` and `render_token_owner_picker` / `prompt_token_owner` (Task 17) accept `Option<u64>` accordingly; tag with "you" only when `Some(id) == u.id`. Also adjust the test in Task 17 to use `Some(938493)` for the existing assertion.)

Thread `store_plans` into the existing `create::create_hook` loop (Task 20).

- [ ] **Step 5: Run the test, expect pass**

Run: `cargo test --test cli_deploy deploy_resolves_templates_and_prompts_for_token_owner`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cli/deploy/store_extensions.rs src/cli/deploy/run.rs tests/cli_deploy.rs
git commit -m "deploy: pre-pass resolves template URLs + prompts for token_owner"
```

---

## Task 20: Deploy bootstrap — branch in `create_hook`

**Files:**
- Modify: `src/cli/deploy/create.rs`

Mirror Task 15's same-env push dispatch on the cross-env create path. The orphan check uses tgt's `/hooks`; the template URL substitution comes from the `StorePlan` produced in Task 19.

- [ ] **Step 1: Failing assertion in the existing `deploy_resolves_templates_and_prompts_for_token_owner` test from Task 19**

Extend the assertion at the end of the test to verify:

```rust
// After the deploy, the tgt hook is recorded in the tgt lockfile with a
// PATCH-applied state matching the src customisation.
let tgt_lf = project.load_lockfile("prod");
let entry = &tgt_lf.objects["hooks"]["master-data-hub"];
assert!(entry.id > 0);
```

- [ ] **Step 2: Run, expect fail (test currently passes the pre-pass but the bootstrap still uses POST /hooks/)**

Run: `cargo test --test cli_deploy deploy_resolves_templates_and_prompts_for_token_owner`
Expected: FAIL (the install POST mounts at `/hooks/create` but the create_hook in deploy/create.rs still routes to `/hooks/`).

- [ ] **Step 3: Implement the branch**

First, update `shape_create_body` in `src/cli/deploy/create.rs` so it threads an explicit-subs map into `rewrite_urls`:

```rust
fn shape_create_body(
    raw: Value,
    kind: &str,
    overlay_paths: Option<&std::collections::BTreeMap<String, Value>>,
    src_lockfile: &Lockfile,
    tgt_lockfile: &Lockfile,
    mapping: &Mapping,
    explicit_subs: &std::collections::BTreeMap<String, String>,
) -> Value {
    let mut payload = raw;
    rewrite_urls(&mut payload, src_lockfile, tgt_lockfile, mapping, explicit_subs);
    if let Some(p) = overlay_paths {
        apply_overrides(&mut payload, p);
    }
    strip_for_create(&mut payload, kind);
    payload
}
```

Update every existing call inside this file to pass `&std::collections::BTreeMap::new()` as the new last arg.

Now rewrite `create_hook`. It gains a `store_plan: Option<&StorePlan>` parameter and `tgt_client` access for the orphan-check list call:

```rust
pub async fn create_hook(
    ctx: &mut CreateCtx<'_>,
    slug: &str,
    store_plan: Option<&crate::cli::deploy::store_extensions::StorePlan>,
    remote_hooks_cache: &mut Option<Vec<crate::model::Hook>>,
) -> Result<()> {
    let payload = read_hook_value(&ctx.src_paths.hooks_dir(), slug)
        .with_context(|| format!("reading src hook '{slug}'"))?;
    let overlay_paths = ctx.tgt_overlay.as_ref().and_then(|ov| ov.hook(slug));

    let mut explicit_subs = std::collections::BTreeMap::new();
    if let Some(plan) = store_plan {
        explicit_subs.insert(plan.src_template_url.clone(), plan.tgt_template_url.clone());
    }

    let created = if let Some(plan) = store_plan {
        // Shape (rewrite URLs incl. template, apply overlay) but DO NOT strip
        // server fields yet — we need the full body to build the install
        // payload and to send the follow-up PATCH.
        let mut body = payload.clone();
        crate::cli::deploy::common::rewrite_urls(
            &mut body, ctx.src_lockfile, ctx.tgt_lockfile, ctx.mapping, &explicit_subs,
        );
        if let Some(p) = overlay_paths {
            crate::overlay::apply_overrides(&mut body, p);
        }
        // The pre-pass already wrote the resolved token_owner into the
        // tgt overlay (per-hook or [defaults]); apply_overrides above
        // picked it up. Confirm:
        if let Some(obj) = body.as_object_mut() {
            obj.insert("token_owner".into(), serde_json::Value::String(plan.token_owner_url.clone()));
        }

        // Orphan check.
        if remote_hooks_cache.is_none() {
            *remote_hooks_cache = Some(ctx.tgt_client.list_hooks(None).await
                .context("listing tgt hooks for store-extension orphan check")?);
        }
        let remote = remote_hooks_cache.as_ref().unwrap();
        let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let installed_id = match crate::cli::deploy::store_extensions::find_orphan(remote, &name, &plan.tgt_template_url) {
            Some(orphan) => {
                eprintln!("adopting orphan store-extension hooks/{slug} (id {}) on tgt", orphan.id);
                orphan.id
            }
            None => {
                let install_body = crate::cli::deploy::store_extensions::build_install_body(&body)?;
                let installed = ctx.tgt_client.create_hook_via_install(&install_body, None).await
                    .with_context(|| format!("POST /hooks/create (installing store extension '{slug}' on tgt)"))?;
                installed.id
            }
        };

        // Reconcile PATCH with the full body.
        let typed: crate::model::Hook = serde_json::from_value(body)
            .with_context(|| format!("deserializing reconcile body for '{slug}'"))?;
        ctx.tgt_client.update_hook(installed_id, &typed, None).await
            .with_context(|| format!("PATCH /hooks/{installed_id} (reconciling store extension '{slug}')"))?
    } else {
        // Existing regular-hook POST path.
        let body = shape_create_body(
            payload, "hooks", overlay_paths,
            ctx.src_lockfile, ctx.tgt_lockfile, ctx.mapping, &explicit_subs,
        );
        ctx.tgt_client.create_hook(&body, None).await
            .with_context(|| format!("POST /hooks (creating '{slug}')"))?
    };

    // Disk + lockfile + mapping (unchanged from the pre-existing path).
    let tgt_hooks_dir = ctx.tgt_paths.hooks_dir();
    std::fs::create_dir_all(&tgt_hooks_dir)
        .with_context(|| format!("creating {}", tgt_hooks_dir.display()))?;
    let json_bytes = write_hook(&tgt_hooks_dir, slug, &created)?;
    let code = created.config.get("code").and_then(|v| v.as_str()).map(|s| s.to_string());
    let h = hook_combined_hash(&json_bytes, &code);
    ctx.tgt_lockfile.upsert("hooks", slug, ObjectEntry {
        id: created.id,
        url: Some(created.url.clone()),
        modified_at: created.modified_at().map(|s| s.to_string()),
        content_hash: Some(h),
    });
    ctx.mapping.hooks.insert(slug.to_string(), slug.to_string());
    Ok(())
}
```

Update the call site of `create_hook` in the bootstrap loop (in whichever file invokes it) to thread the `store_plans` lookup and the `remote_hooks_cache`:

```rust
let mut remote_hooks_cache: Option<Vec<crate::model::Hook>> = None;
for slug in hooks_to_create {
    let plan = store_plans.iter().find(|p| p.src_slug == slug);
    create_hook(&mut ctx, &slug, plan, &mut remote_hooks_cache).await?;
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test --test cli_deploy`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/deploy/create.rs
git commit -m "deploy: create_hook branches to two-call install for store extensions"
```

---

## Task 21: Non-TTY refusal when overlay token_owner is missing

**Files:**
- Modify: `src/cli/deploy/store_extensions.rs`
- Modify: `tests/cli_deploy.rs`

- [ ] **Step 1: Failing test**

```rust
#[tokio::test]
async fn deploy_refuses_non_tty_without_token_owner_overlay() {
    let project = TestProject::new();
    let src = MockServer::start().await;
    let tgt = MockServer::start().await;
    // ... same setup as Task 19 but DO NOT write the tgt overlay ...
    // CI mode = --yes (no TTY), no overlay entry → refusal expected.

    let assertion = project.run(&["deploy", "test", "prod", "--yes"]).assert().failure();
    assertion.stderr(predicate::str::contains("token_owner"))
             .stderr(predicate::str::contains("master-data-hub"))
             .stderr(predicate::str::contains("envs/prod/overlay.toml"));

    // Confirm no remote writes occurred.
    tgt.received_requests().await
        .iter()
        .filter(|r| matches!(r.method, wiremock::http::Method::POST | wiremock::http::Method::PATCH))
        .for_each(|r| panic!("unexpected mutating request: {} {}", r.method, r.url.path()));
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test --test cli_deploy deploy_refuses_non_tty_without_token_owner_overlay`
Expected: FAIL.

- [ ] **Step 3: Implement the refusal**

In `plan_store_extension_bootstrap` (Task 19), when `interactive == false` and `effective_token_owner(...)` returns `None`:

```rust
if !interactive {
    return Err(anyhow!(
        "deploy needs token_owner for store extension '{src_slug}' on {tgt_env_label}, but {} has no [hooks.{src_slug}] token_owner and no [defaults] store_extension_token_owner.\nRun 'rdc deploy {src} {tgt_env_label}' on a TTY once to pick interactively, or edit the overlay directly. Aborting before any remote writes.",
        tgt_overlay_path.display()
    ));
}
```

The error must surface before any tgt-side write. The pre-pass should run before the bootstrap loop.

- [ ] **Step 4: Run, expect pass**

Run: `cargo test --test cli_deploy deploy_refuses_non_tty_without_token_owner_overlay`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/deploy/store_extensions.rs tests/cli_deploy.rs
git commit -m "deploy: refuse non-TTY runs missing token_owner overlay"
```

---

## Task 22: Template-unavailable failure mode

**Files:**
- Modify: `tests/cli_deploy.rs`

- [ ] **Step 1: Failing test**

```rust
#[tokio::test]
async fn deploy_errors_when_template_missing_on_tgt() {
    // Same setup as Task 19, but the tgt /hook_templates list is empty.
    // ... mount tgt /hook_templates returning [] ...
    let assertion = project.run(&["deploy", "test", "prod", "--yes"]).assert().failure();
    assertion.stderr(predicate::str::contains("Master Data Hub"));
    assertion.stderr(predicate::str::contains("not available on prod"));
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test cli_deploy deploy_errors_when_template_missing_on_tgt`
Expected: PASS — Task 14's `build_template_url_map` already produces this error, and Task 19 propagates it before any writes.

- [ ] **Step 3: Commit**

```bash
git add tests/cli_deploy.rs
git commit -m "deploy: regression test for missing-template-on-tgt error"
```

---

## Task 23: `rdc status` surfaces store-extension count

**Files:**
- Modify: `src/cli/status.rs`

- [ ] **Step 1: Failing test**

Locate the existing `tests/cli_status.rs` shape; add:

```rust
#[tokio::test]
async fn status_shows_store_extension_count() {
    let project = TestProject::new();
    let server = MockServer::start().await;
    project.write_rdc_toml(&[("test", &server.uri())]);
    project.write_token("test", "TKN");

    // Lockfile entries for two hooks (just id + url; lockfile has no
    // extension_source field — status reads it from the snapshot JSON).
    project.write_lockfile_entry("test", "hooks", "validator-invoices", 1,
        &format!("{}/hooks/1", server.uri()));
    project.write_lockfile_entry("test", "hooks", "master-data-hub", 2,
        &format!("{}/hooks/2", server.uri()));

    // Snapshot files — one regular, one store.
    project.write_hook_file("test", "validator-invoices", &serde_json::json!({
        "name": "Validator", "type": "function", "extension_source": "custom"
    }));
    project.write_hook_file("test", "master-data-hub", &serde_json::json!({
        "name": "Master Data Hub", "type": "webhook",
        "extension_source": "rossum_store",
        "hook_template": format!("{}/hook_templates/39", server.uri())
    }));

    // Mock /organizations/* for the auth check.
    Mock::given(method("GET")).and(path_regex(r"^/organizations/\d+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&serde_json::json!({})))
        .mount(&server).await;

    let out = project.run(&["status", "test"]).assert().success().get_output().stdout.clone();
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("(1 store extension)"));
}
```

(The lockfile alone doesn't carry `extension_source` — that field lives in the snapshot JSON. Status reads each hook file's JSON to count store extensions.)

- [ ] **Step 2: Run, expect fail**

Run: `cargo test --test cli_status status_shows_store_extension_count`
Expected: FAIL.

- [ ] **Step 3: Implement**

In `src/cli/status.rs`, where the lockfile-summary line is built, add a pre-step that reads each `hooks/<slug>.json`, parses minimally for `extension_source`, and counts the `"rossum_store"` ones. Add to the summary line when non-zero: `"  (N store extension(s))"`.

Use `serde_json::from_str::<serde_json::Value>` for the minimal read — avoids round-tripping the whole `Hook` struct.

- [ ] **Step 4: Run, expect pass**

Run: `cargo test --test cli_status status_shows_store_extension_count`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/status.rs tests/cli_status.rs
git commit -m "status: show '(N store extension)' count on lockfile summary line"
```

---

## Task 24: Deploy plan-output sub-line

**Files:**
- Modify: `src/cli/deploy/run.rs` (or wherever the plan summary is printed)

- [ ] **Step 1: Failing test in `tests/cli_deploy.rs`**

```rust
#[tokio::test]
async fn deploy_plan_lists_store_extensions_in_dry_run() {
    // ... same setup as Task 19 with one store extension and one regular hook in src ...
    // Use --dry-run so no writes happen.
    let out = project.run(&["deploy", "test", "prod", "--dry-run"]).assert().success().get_output().stdout.clone();
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("are store extensions"));
    assert!(s.contains("master-data-hub"));
}
```

- [ ] **Step 2: Run, expect fail**

Run: `cargo test --test cli_deploy deploy_plan_lists_store_extensions_in_dry_run`
Expected: FAIL.

- [ ] **Step 3: Implement the sub-line**

In the plan printer, after the `+ create: ...` line, when `store_plans` is non-empty:

```rust
println!(
    "             ↳ {} of the {} hooks are store extensions (POST /hooks/create + PATCH each):",
    store_plans.len(),
    total_hooks_to_create,
);
for plan in &store_plans {
    let tgt_id = plan.tgt_template_url.rsplit('/').next().unwrap_or("?");
    println!("                 {}  → template '{}' on {} (id {})",
             plan.tgt_slug, plan.tgt_template_name, tgt_env_label, tgt_id);
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cargo test --test cli_deploy deploy_plan_lists_store_extensions_in_dry_run`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/deploy/run.rs tests/cli_deploy.rs
git commit -m "deploy: plan output surfaces store-extension creates"
```

---

## Task 25: Documentation — README + spec link

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the README's "File layout" / "Pull" / "Deploy" sections**

Add a paragraph in the appropriate section describing the store-extension experience:

```markdown
### Store extensions (Master Data Hub, Email Notifications, …)

Extensions installed via the Rossum store live under `hooks/` like every
other hook, marked by `"extension_source": "rossum_store"` in the JSON.
`rdc push` knows to use the store install endpoint to create them on a
fresh environment; `rdc deploy` resolves the target cluster's matching
template by name and prompts once for the target's service-account
`token_owner`, saving the choice to `envs/<env>/overlay.toml`. Update,
delete, drift detection, and conflict resolution are identical to a
regular hook.

For automated CI deploys, set the `token_owner` in `envs/<env>/overlay.toml`
manually (either per-hook under `[hooks.<slug>]` or as a default under
`[defaults] store_extension_token_owner`); `rdc deploy --yes` will refuse
to start without it.
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: README section for store extensions"
```

---

## Spec Self-Review

After implementing all 25 tasks, the spec's coverage is:

| Spec section | Implemented by |
|---|---|
| Detection (extension_source marker) | Task 1 |
| Hook accessors | Task 1 |
| Snapshot (full body round-tripped) | No change required; the existing `extra` map already round-trips |
| Push two-call create + orphan recovery | Tasks 11, 12, 13, 15, 16 |
| Deploy URL rewriting for hook_template | Task 9, 14, 19, 20 |
| Deploy token_owner overlay + interactive prompt | Tasks 8, 10, 17, 18, 19 |
| Deploy non-TTY refusal | Task 21 |
| Deploy template-unavailable error | Task 22 (validates Task 14 wiring) |
| `[hook_templates]` mapping cache | Task 7, 19 |
| `[defaults]` overlay section | Task 8 |
| status surface | Task 23 |
| deploy plan surface | Task 24 |
| Documentation | Task 25 |
| API methods (3 new) | Tasks 4, 5, 6 |
| Models (HookTemplate, User) | Tasks 2, 3 |
