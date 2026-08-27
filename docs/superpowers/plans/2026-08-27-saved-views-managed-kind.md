# Saved views as a managed kind — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Rossum `/v1/saved_views` a fully managed rdc kind — pulled, pushed, deleted and promoted between envs — with the managed set narrowed to shared views only.

**Architecture:** `saved_views` becomes a flat, org-scoped kind modelled on `labels`: one JSON file per view at `envs/<env>/saved-views/<slug>.json`, no sidecars. The pull driver filters `shared == true` client-side (the server's `?shared=` is silently ignored), which is the safety boundary keeping users' private views and the customer data inside their `query` out of git. Refs inside `queues_filter` and `query` portabilize for free through the existing recursive URL walker; a ref that cannot cross an env is a hard error rather than a silent rewrite, because saved views deliberately do not participate in deferred relink.

**Tech Stack:** Rust (edition 2024), `serde_json` with `preserve_order`, `indexmap`, `wiremock` + `assert_cmd` for integration tests, `tokio` test runtime.

**Spec:** `docs/superpowers/specs/2026-08-27-saved-views-managed-kind-design.md`

## Global Constraints

- Kind string is `saved_views` (lockfile / mapping / overlay key). On-disk directory is `saved-views` (hyphenated). These differ on purpose — it matches `email_templates` / `email-templates`.
- rdc manages `shared == true` ONLY. A private view is never snapshotted, pushed or deleted.
- **No `LOCKFILE_VERSION` bump. No new `rdc.toml` key. No new CLI flag.**
- Verified API facts this plan depends on: `name` `max_length` **255**; `query` and `name` are the only required POST fields; `id`, `url`, `organization`, `created_by`, `created_at`, `modified_by`, `modified_at` are read-only and overrides are silently ignored; `name` is **not unique**; DELETE returns 204 and is a hard delete; refs inside `query` MUST be full URLs (a bare int is `400 Incorrect type. Expected URL string, received int.`); `field.<schema_id>` keys are NOT validated; list filters `?shared=` / `?created_by=` are **silently ignored** while `?ordering=` works.
- Never put a customer name or customer-specific identifier in code, tests, fixtures, docs **or commit messages**. Use `acme`, `main`, `invoices`, `test`/`dev`/`prod`, `field_a`, `document_id`.
- **Build economy:** this crate is slow to compile. Per task run only the filtered test (`cargo test --lib <filter>` or `cargo test --test <bin> <filter>`). The whole suite runs once, in Task 10. Never start a rebuild while an integration suite is running — it swaps `target/debug/rdc` under the tests that spawn it.
- **Never run repo-wide `cargo fmt`.** This crate is not fmt-clean under current rustfmt and a repo-wide run would produce an enormous unrelated diff. Match surrounding style by hand.
- Commit to local `main`. **Never `git push`.**
- Every commit message ends with `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`.

---

### Task 1: Model and API client methods

The typed object plus the three HTTP methods. There is deliberately **no** `delete_saved_view`: `push::deletes` issues the actual DELETE through the generic `client.delete_path(&format!("/{kind}/{id}"))`, so the kind string alone reaches `/saved_views/{id}`.

**Files:**
- Create: `src/model/saved_view.rs`
- Modify: `src/model/mod.rs` (add `pub mod saved_view;` and `pub use saved_view::SavedView;` in the existing alphabetical lists)
- Modify: `src/api/mod.rs` (add `SavedView` to the `crate::model::{…}` import; add one `list_`, one `create_`, one `update_` method beside the label ones)

**Interfaces:**
- Consumes: nothing.
- Produces: `crate::model::SavedView` with public fields `id: u64`, `url: String`, `name: String`, `shared: bool`, `queues_filter: Vec<String>`, `query: serde_json::Value`, `extra: IndexMap<String, Value>`, and methods `modified_at() -> Option<&str>` / `modified_by() -> Option<&str>`. Plus `RossumClient::list_saved_views(ProgressHandle) -> Result<Vec<SavedView>>`, `create_saved_view(&Value, ProgressHandle) -> Result<SavedView>`, `update_saved_view(u64, &SavedView, ProgressHandle) -> Result<SavedView>`. Every later task uses these.

- [ ] **Step 1: Write the failing test**

Create `src/model/saved_view.rs` containing ONLY the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn round_trip_preserves_unknown_fields() {
        let payload = json!({
            "id": 42,
            "url": "https://acme.rossum.app/api/v1/saved_views/42",
            "name": "Awaiting approval",
            "shared": true,
            "queues_filter": ["https://acme.rossum.app/api/v1/queues/100"],
            "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
            "organization": "https://acme.rossum.app/api/v1/organizations/1",
            "created_by": "https://acme.rossum.app/api/v1/users/7",
            "created_at": "2026-08-01T08:00:00Z",
            "modified_at": "2026-08-02T09:00:00Z",
            "modified_by": "https://acme.rossum.app/api/v1/users/8"
        });
        let v: SavedView = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(v.id, 42);
        assert_eq!(v.name, "Awaiting approval");
        assert!(v.shared);
        assert_eq!(v.queues_filter.len(), 1);
        assert_eq!(v.modified_at(), Some("2026-08-02T09:00:00Z"));
        // The forward-compat bucket must keep every key the struct does not name.
        let round_trip = serde_json::to_value(&v).unwrap();
        assert_eq!(round_trip, payload);
    }

    #[test]
    fn null_id_and_url_deserialize_as_defaults() {
        // A hand-scaffolded new-object file carries nulls; the create path
        // strips both before POST anyway.
        let payload = json!({
            "id": null, "url": null, "name": "New view",
            "query": { "$and": [] }
        });
        let v: SavedView = serde_json::from_value(payload).unwrap();
        assert_eq!(v.id, 0);
        assert_eq!(v.url, "");
        assert!(!v.shared, "shared must default to false when absent");
        assert!(v.queues_filter.is_empty());
    }

    #[test]
    fn absent_query_defaults_to_null() {
        let payload = json!({ "id": 1, "url": "u", "name": "n" });
        let v: SavedView = serde_json::from_value(payload).unwrap();
        assert_eq!(v.query, serde_json::Value::Null);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib saved_view`
Expected: FAIL — `cannot find type SavedView in this scope` (the struct does not exist yet).

- [ ] **Step 3: Write the minimal implementation**

Prepend to `src/model/saved_view.rs`, above the test module:

```rust
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Rossum saved view — a stored annotation-dashboard filter.
///
/// rdc manages SHARED views only; `cli::pull::saved_views::list` drops the rest
/// and the design doc's section B explains why that filter is a safety boundary
/// rather than a convenience.
///
/// Field declaration order IS the on-disk key order: `serde_json` is built with
/// `preserve_order`, so `to_value` emits fields in this order and the codec
/// writes them out unchanged. Scalars first, the `query` blob last, so a diff of
/// the interesting fields stays readable.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct SavedView {
    #[serde(default, deserialize_with = "crate::model::null_as_default")]
    pub id: u64,
    #[serde(default, deserialize_with = "crate::model::null_as_default")]
    pub url: String,
    pub name: String,
    #[serde(default)]
    pub shared: bool,
    #[serde(default)]
    pub queues_filter: Vec<String>,
    #[serde(default)]
    pub query: Value,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl SavedView {
    pub fn modified_at(&self) -> Option<&str> {
        crate::model::modified_at(&self.extra)
    }

    pub fn modified_by(&self) -> Option<&str> {
        crate::model::modified_by(&self.extra)
    }
}
```

In `src/model/mod.rs` add to the two existing lists, keeping them alphabetical:

```rust
pub mod saved_view;
```
```rust
pub use saved_view::SavedView;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib saved_view`
Expected: PASS (3 tests).

- [ ] **Step 5: Add the API client methods**

In `src/api/mod.rs`, add `SavedView` to the existing `use crate::model::{…}` list. Then add one method to each of the three existing blocks, beside the label equivalents:

```rust
    pub async fn list_saved_views(&self, progress: ProgressHandle) -> Result<Vec<SavedView>> {
        self.list_paginated("/saved_views", progress).await
    }
```
```rust
    pub async fn create_saved_view(&self, body: &serde_json::Value, progress: ProgressHandle) -> Result<SavedView> {
        self.post_json("/saved_views", body, progress).await
    }
```
```rust
    /// `PATCH /saved_views/{id}`.
    ///
    /// There is no `delete_saved_view`: `push::deletes` issues DELETE through
    /// the generic `delete_path("/{kind}/{id}")`, and the kind string
    /// `saved_views` is already the correct path segment.
    pub async fn update_saved_view(&self, id: u64, view: &SavedView, progress: ProgressHandle) -> Result<SavedView> {
        self.patch_json(&format!("/saved_views/{id}"), view, progress).await
    }
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo test --lib saved_view`
Expected: PASS, no warnings about the new methods (they are `pub`).

- [ ] **Step 7: Commit**

```bash
git add src/model/saved_view.rs src/model/mod.rs src/api/mod.rs
git commit -m "$(cat <<'MSG'
feat(saved-views): add the SavedView model and API client methods

First slice of making /v1/saved_views a managed kind. Field order in the struct
is the on-disk key order (serde_json preserve_order), so scalars come first and
the query blob last.

No delete_saved_view: push::deletes issues DELETE through the generic
delete_path("/{kind}/{id}"), and "saved_views" is already the right segment.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 2: Paths, overlay section, and the codec

The codec is the single source of truth for on-disk bytes. Its `overlay()` method needs an `Overlay::saved_view` accessor to compile, so the overlay field and accessor land here; wiring `saved_views` into `Overlay::kind_maps()` is Task 7's job.

**Files:**
- Modify: `src/paths.rs` (add `saved_views_dir()` beside `labels_dir()`, plus a test)
- Modify: `src/overlay.rs` (add the `saved_views` field and the `saved_view()` accessor)
- Create: `src/snapshot/codec/saved_views.rs`
- Modify: `src/snapshot/codec/mod.rs` (add `mod saved_views;` and the registry arm)
- Modify: `tests/codec_invariant.rs` (add `sample_saved_view()` and include it wherever the file iterates kinds)

**Interfaces:**
- Consumes: `crate::model::SavedView` (Task 1).
- Produces: `Paths::saved_views_dir() -> PathBuf`, `Overlay::saved_view(&str) -> Option<&BTreeMap<String, Value>>`, and `codec("saved_views")` returning a working `&dyn KindCodec`. Tasks 4, 5, 6, 7 and 8 all call these.

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `src/paths.rs`:

```rust
    #[test]
    fn saved_views_dir_path() {
        assert_eq!(p().saved_views_dir(), Path::new("/proj/envs/dev/saved-views"));
    }
```

Create `src/snapshot/codec/saved_views.rs` with ONLY the tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::codec::KindCodec;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "id": 42,
            "url": "https://acme.rossum.app/api/v1/saved_views/42",
            "name": "Awaiting approval",
            "shared": true,
            "queues_filter": ["rdc://queues/invoices"],
            "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
            "organization": "https://acme.rossum.app/api/v1/organizations/1",
            "created_by": "https://acme.rossum.app/api/v1/users/7",
            "created_at": "2026-08-01T08:00:00Z",
            "modified_at": "2026-08-02T09:00:00Z",
            "modified_by": "https://acme.rossum.app/api/v1/users/8"
        })
    }

    #[test]
    fn strips_stamps_and_creator_from_disk() {
        let art = SavedViews.disk_bytes(&sample()).unwrap();
        let s = std::str::from_utf8(&art.json).unwrap();
        for gone in ["modified_at", "modified_by", "created_by", "created_at"] {
            assert!(!s.contains(gone), "{gone} must be stripped from disk; got:\n{s}");
        }
    }

    #[test]
    fn keeps_managed_fields_on_disk() {
        let art = SavedViews.disk_bytes(&sample()).unwrap();
        let s = std::str::from_utf8(&art.json).unwrap();
        for kept in ["name", "shared", "queues_filter", "query", "organization"] {
            assert!(s.contains(kept), "{kept} must survive on disk; got:\n{s}");
        }
    }

    #[test]
    fn no_sidecars() {
        assert!(SavedViews.disk_bytes(&sample()).unwrap().sidecars.is_empty());
    }

    #[test]
    fn path_is_under_saved_views_dir() {
        use crate::paths::Paths;
        let paths = Paths::for_env("/proj", "dev");
        assert_eq!(
            SavedViews.path(&paths, "awaiting-approval"),
            std::path::PathBuf::from("/proj/envs/dev/saved-views/awaiting-approval.json")
        );
    }

    /// The `created_by` strip is scoped to THIS codec. Widening the global
    /// HIDDEN_FIELDS would rewrite every other kind's on-disk bytes and churn
    /// every existing project on its next sync.
    #[test]
    fn created_by_strip_is_not_global() {
        // Compare as a slice: HIDDEN_FIELDS is `&[&str]`, so a bare array
        // literal on the right-hand side would not unify.
        assert_eq!(
            crate::snapshot::key_order::HIDDEN_FIELDS,
            &["modified_at", "modified_by"][..],
            "HIDDEN_FIELDS must stay exactly these two"
        );
        let label = json!({
            "id": 1, "url": "u", "name": "n",
            "created_by": "https://acme.rossum.app/api/v1/users/7"
        });
        let art = crate::snapshot::codec::codec("labels").unwrap().disk_bytes(&label).unwrap();
        let s = std::str::from_utf8(&art.json).unwrap();
        assert!(s.contains("created_by"), "labels must still keep created_by on disk");
    }

    /// Deferred relink is unsafe for this kind, so the on-disk shape must keep
    /// `queues_filter` as a real array of refs the resolver can see.
    #[test]
    fn queues_filter_refs_survive_as_strings() {
        let art = SavedViews.disk_bytes(&sample()).unwrap();
        let v: Value = serde_json::from_slice(&art.json).unwrap();
        assert_eq!(v["queues_filter"][0], json!("rdc://queues/invoices"));
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib saved_views` and `cargo test --lib saved_views_dir_path`
Expected: FAIL — `cannot find value SavedViews`, `no method named saved_views_dir`.

- [ ] **Step 3: Implement paths and the overlay accessor**

In `src/paths.rs`, directly after `labels_dir()`:

```rust
    /// `<root>/envs/<env>/saved-views/`. Flat, one file per view: a saved view
    /// is org-scoped, and its `queues_filter` is a 0..n list, so there is no
    /// single owning queue to nest under.
    pub fn saved_views_dir(&self) -> PathBuf {
        self.env_root().join("saved-views")
    }
```

In `src/overlay.rs`, add the field alongside the other kind sections:

```rust
    /// Saved-view overrides keyed by saved-view slug. The common use is a
    /// per-env `query`: an override replaces the whole object, which is the
    /// documented escape hatch when a source `query` carries a ref that cannot
    /// cross into this env (see `migrate`'s saved-view ref validation).
    #[serde(default)]
    pub saved_views: BTreeMap<String, BTreeMap<String, Value>>,
```

Add it to `impl Default for Overlay` (`saved_views: BTreeMap::new(),`), and add the accessor beside `label()`:

```rust
    pub fn saved_view(&self, slug: &str) -> Option<&BTreeMap<String, Value>> {
        self.saved_views.get(slug)
    }
```

- [ ] **Step 4: Implement the codec**

Prepend to `src/snapshot/codec/saved_views.rs`:

```rust
//! [`KindCodec`] implementation for the `saved_views` kind.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::overlay::Overlay;
use crate::paths::Paths;
use crate::snapshot::codec::{DiskArtifact, KindCodec};
use crate::snapshot::create::{strip_for_create, strip_for_cross_env_patch};
use crate::snapshot::key_order::strip_hidden_fields;

pub struct SavedViews;

impl KindCodec for SavedViews {
    fn kind(&self) -> &'static str {
        "saved_views"
    }

    fn disk_bytes(&self, value: &Value) -> anyhow::Result<DiskArtifact> {
        let mut v = value.clone();
        strip_hidden_fields(&mut v);
        // `created_by` is an env-specific USER url, and users are not a
        // snapshotted kind, so it never portabilizes — the same leak class as a
        // queue's `rir_url` or a hook's `token_owner`. `created_at` is pure
        // churn. Both are stripped HERE rather than added to the global
        // HIDDEN_FIELDS, which would change the on-disk bytes of every other
        // kind that carries them and force a rewrite of every existing project.
        if let Some(obj) = v.as_object_mut() {
            obj.shift_remove("created_by");
            obj.shift_remove("created_at");
        }
        let mut json = serde_json::to_vec_pretty(&v)?;
        json.push(b'\n');
        Ok(DiskArtifact { json, sidecars: vec![] })
    }

    fn create_body(&self, body: &mut Value) {
        strip_for_create(body, "saved_views");
    }

    fn cross_env_body(&self, body: &mut Value) {
        strip_for_cross_env_patch(body, "saved_views");
    }

    fn overlay<'a>(&self, overlay: &'a Overlay, slug: &str) -> Option<&'a BTreeMap<String, Value>> {
        overlay.saved_view(slug)
    }

    fn path(&self, paths: &Paths, slug: &str) -> PathBuf {
        paths.saved_views_dir().join(format!("{slug}.json"))
    }
}
```

In `src/snapshot/codec/mod.rs` add the module declaration in the alphabetical list and the registry arm:

```rust
mod saved_views;
```
```rust
        "saved_views" => Some(&saved_views::SavedViews),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib saved_views` then `cargo test --lib paths`
Expected: PASS.

If `HIDDEN_FIELDS` is not public, make it `pub` (it is already `pub const` — verify with `grep -n "pub const HIDDEN_FIELDS" src/snapshot/key_order.rs`).

- [ ] **Step 6: Extend the cross-kind codec invariant test**

In `tests/codec_invariant.rs`, add a sample beside the existing ones:

```rust
fn sample_saved_view() -> serde_json::Value {
    json!({
        "id": 42,
        "url": "https://x/api/v1/saved_views/42",
        "name": "Awaiting approval",
        "shared": true,
        "queues_filter": [],
        "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
        "created_by": "https://x/api/v1/users/7",
        "created_at": "2026-08-01T08:00:00Z",
        "modified_at": "2026-08-02T09:00:00Z"
    })
}
```

Then find every place the file enumerates `(kind, sample)` pairs and add `("saved_views", sample_saved_view())`:

Run: `grep -n '"labels"' tests/codec_invariant.rs`

Add the saved-views entry at each hit.

- [ ] **Step 7: Run the invariant suite**

Run: `cargo test --test codec_invariant`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/paths.rs src/overlay.rs src/snapshot/codec/saved_views.rs src/snapshot/codec/mod.rs tests/codec_invariant.rs
git commit -m "$(cat <<'MSG'
feat(saved-views): add paths, overlay section and the KindCodec

The codec strips created_by and created_at on top of the usual modified_*
stamps. created_by is an env-specific user URL and users are not a snapshotted
kind, so it can never portabilize -- the same leak class as a queue's rir_url or
a hook's token_owner.

That strip is scoped to this codec on purpose. Adding the fields to the global
HIDDEN_FIELDS would change the on-disk bytes of every other kind carrying them
and force a rewrite of every existing project on its next sync; a test pins
HIDDEN_FIELDS to its current two entries so that cannot happen by accident.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 3: Field limits and the `shared: false` offline refusal

Two guards that both run before any network write. The limits entry also satisfies `every_pushable_kind_has_limits`, which fails the build for a pushable kind with no entry.

`ChangeList.saved_views` does not exist until Task 5, so this task adds the checker as a free function over a parsed `Value` and Task 5 wires the `ChangeList` method. That keeps this task independently testable.

**Files:**
- Modify: `src/snapshot/limits.rs` (add the `field_limits` arm, the `UnsharedSavedView` type, the checker, tests)

**Interfaces:**
- Consumes: nothing.
- Produces: `field_limits("saved_views")` returning `&[("name", 255)]`; `pub struct UnsharedSavedView { pub slug: String, pub path: std::path::PathBuf }`; `pub fn check_saved_view_shared(body: &serde_json::Value) -> bool` returning `true` when the body is managed (i.e. `shared == true`). Task 5 calls both from `ChangeList`.

- [ ] **Step 1: Write the failing tests**

Append inside the existing `#[cfg(test)] mod tests` in `src/snapshot/limits.rs`:

```rust
    #[test]
    fn saved_views_have_a_name_limit() {
        assert_eq!(field_limits("saved_views"), &[("name", 255)]);
    }

    #[test]
    fn saved_view_over_length_name_is_a_violation() {
        let body = serde_json::json!({
            "name": "n".repeat(256),
            "shared": true,
            "query": { "$and": [] }
        });
        let v = check_field_limits("saved_views", &body);
        assert_eq!(v.len(), 1, "a 256-char name must violate the 255 limit");
        assert_eq!(v[0].0, "name");
    }

    #[test]
    fn saved_view_name_at_the_limit_is_accepted() {
        let body = serde_json::json!({
            "name": "n".repeat(255),
            "shared": true,
            "query": { "$and": [] }
        });
        assert!(check_field_limits("saved_views", &body).is_empty());
    }

    #[test]
    fn shared_true_is_managed() {
        let body = serde_json::json!({ "name": "v", "shared": true, "query": {} });
        assert!(check_saved_view_shared(&body));
    }

    #[test]
    fn shared_false_and_absent_are_both_unmanaged() {
        for body in [
            serde_json::json!({ "name": "v", "shared": false, "query": {} }),
            serde_json::json!({ "name": "v", "query": {} }),
            // A non-boolean is not `true`, so it is refused rather than coerced.
            serde_json::json!({ "name": "v", "shared": "yes", "query": {} }),
        ] {
            assert!(
                !check_saved_view_shared(&body),
                "must be refused: {body}"
            );
        }
    }
```

`check_field_limits` is the existing per-kind checker. Confirm its exact name and return shape first:

Run: `grep -n "pub fn check_field_limits" -A 8 src/snapshot/limits.rs`

If the signature differs from `(kind, &Value) -> Vec<(&'static str, usize, usize)>`, adapt the two limit tests above to the real shape — assert only that a 256-char name yields exactly one violation naming `name`, and that 255 yields none.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib limits::tests::saved_view`
Expected: FAIL — `field_limits("saved_views")` returns `&[]` and `check_saved_view_shared` does not exist.

- [ ] **Step 3: Implement**

In `field_limits`, add the arm beside `"labels"`:

```rust
        // Only `name` is capped. `query` has no declared `max_length` (it is a
        // JSON blob), and `shared` / `queues_filter` are not strings.
        "saved_views" => &[("name", 255)],
```

Add the type and checker near the other offline validators:

```rust
/// A local saved-view file rdc refuses to push because it is not shared.
#[derive(Debug, PartialEq, Eq)]
pub struct UnsharedSavedView {
    pub slug: String,
    pub path: std::path::PathBuf,
}

/// True when a saved-view body is one rdc manages — i.e. `shared` is exactly
/// `true`.
///
/// Pushing an unshared view would create an object the pull side immediately
/// filters back out (`cli::pull::saved_views::list` keeps only shared views), so
/// it would be re-created on every sync and never recorded in the lockfile: a
/// create-then-vanish loop with no diagnostic. Refusing offline turns that into
/// one clear message before the first remote write.
///
/// A missing key and a non-boolean both count as unmanaged rather than being
/// coerced — guessing here would push the very object we mean to refuse.
pub fn check_saved_view_shared(body: &serde_json::Value) -> bool {
    body.get("shared").and_then(|s| s.as_bool()) == Some(true)
}
```

Also add `"saved_views"` to the `every_pushable_kind_has_limits` test's kind list.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib limits`
Expected: PASS, including `every_pushable_kind_has_limits`.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/limits.rs
git commit -m "$(cat <<'MSG'
feat(saved-views): add the name limit and the shared-flag offline check

name is capped at 255 per OPTIONS; query has no declared limit because it is a
JSON blob.

check_saved_view_shared refuses a local file that is not shared: true. Pushing
one creates a view the pull side immediately filters back out, so it would be
re-created every sync and never recorded -- a create-then-vanish loop with no
diagnostic. A missing key or a non-boolean counts as unmanaged rather than being
coerced, since guessing would push the object we mean to refuse.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 4: Pull driver and catalog wiring

The shared-only filter lives here. This is the task that decides what rdc can ever see.

**Files:**
- Create: `src/cli/pull/saved_views.rs`
- Modify: `src/cli/pull/mod.rs` (add `pub mod saved_views;`)
- Modify: `src/cli/pull/common.rs` (`RemoteCatalog` field; the private `Listed` enum variant; the private `Kind` enum variant; the `kinds` array; the list dispatch arm; the local `Option<Vec<…>>` accumulator and its unwrap into the catalog)
- Modify: `src/cli/pull/portabilize.rs` (`locate_json_path` arm)

**Interfaces:**
- Consumes: `SavedView` (Task 1), `codec("saved_views")` and `Paths::saved_views_dir()` (Task 2).
- Produces: `pull::saved_views::list(&PullCtx, &Arc<Log>) -> Result<Vec<SavedView>>` (already filtered to shared) and `pull::saved_views::process(&mut PullCtx, Vec<SavedView>, &BTreeSet<(String,String)>, &Arc<Log>) -> Result<(usize, usize)>`; `RemoteCatalog.saved_views: Vec<SavedView>`. Tasks 5 and 6 consume both.

- [ ] **Step 1: Register the new list endpoint in EVERY mock array — do this FIRST**

This step lives here rather than in Task 9 (ruling R2): Task 4 is what makes rdc
issue `GET /saved_views` on every sync, so registering the mock route any later
would leave `cargo test --test cli_sync` red across Tasks 4-8 and break the
plan's own "each task independently testable" contract.

`rdc` now issues `GET /saved_views` on every sync. Any test whose mock server
does not answer that path gets a wiremock 404 and fails. There are **11** such
arrays, and they are not all one constant:

Run: `grep -rn '"/api/v1/email_templates",' tests/cli_sync.rs tests/cli_doctor.rs`

That prints one line per array (`email_templates` is a reliable proxy — every
array contains it). Add `"/api/v1/saved_views",` to each. Expect hits at roughly
`tests/cli_sync.rs:109`, `:209`, `:3783`, `:4121`, `:4195`, `:4347`, `:7582`,
`:10528`, `:11485`, `:11653` and `tests/cli_doctor.rs:25`.

`tests/cli_sync.rs:198` is the shared const and its length annotation must grow:

```rust
const CORE_LIST_ENDPOINTS: [&str; 12] = [
```

with `"/api/v1/saved_views",` added to the body. The compiler catches the length
mismatch; it cannot catch a missed inline array, so work from the grep output and
re-run it afterwards to confirm every line has a saved-views sibling.

Registering the route is additive and safe to do before the driver exists: rdc
does not call it yet, so the suite must stay green.

Run: `cargo test --test cli_sync`
Expected: PASS (unchanged behaviour — this is pure harness preparation).

- [ ] **Step 2: Write the failing test**

Create `src/cli/pull/saved_views.rs` with the tests only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::RossumClient;
    use crate::paths::Paths;
    use crate::state::Lockfile;
    use serde_json::json;

    fn mk(id: u64, name: &str, shared: bool) -> SavedView {
        SavedView {
            id,
            url: format!("https://example.invalid/api/v1/saved_views/{id}"),
            name: name.to_string(),
            shared,
            queues_filter: Vec::new(),
            query: json!({ "$and": [] }),
            extra: indexmap::IndexMap::new(),
        }
    }

    #[tokio::test]
    async fn process_writes_only_subset_members() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let mut lockfile = Lockfile::default();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);

        let mut ctx = PullCtx {
            paths: &paths,
            client: &client,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };

        let views = vec![mk(1, "in scope", true), mk(2, "out of scope", true)];
        let mut subset = BTreeSet::new();
        subset.insert(("saved_views".to_string(), "in-scope".to_string()));

        let (written, conflicts) = process(&mut ctx, views, &subset, &progress).await.unwrap();

        assert_eq!(written, 1);
        assert_eq!(conflicts, 0);
        assert!(paths.saved_views_dir().join("in-scope.json").exists());
        assert!(!paths.saved_views_dir().join("out-of-scope.json").exists());
    }

    /// Two views with the same name are routine: the API does not enforce
    /// uniqueness, and per-user namespaces make collisions common.
    #[tokio::test]
    async fn duplicate_names_get_suffixed_slugs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let mut lockfile = Lockfile::default();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let mut ctx = PullCtx {
            paths: &paths,
            client: &client,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };

        let views = vec![mk(1, "Shared filter", true), mk(2, "Shared filter", true)];
        let mut subset = BTreeSet::new();
        subset.insert(("saved_views".to_string(), "shared-filter".to_string()));
        subset.insert(("saved_views".to_string(), "shared-filter-2".to_string()));

        let (written, _) = process(&mut ctx, views, &subset, &progress).await.unwrap();
        assert_eq!(written, 2);
        assert!(paths.saved_views_dir().join("shared-filter.json").exists());
        assert!(paths.saved_views_dir().join("shared-filter-2.json").exists());
    }

    /// The entire cross-env story for this kind rests on the generic walker
    /// reaching refs nested inside `query`. Pin it rather than trusting that it
    /// "comes for free": portabilize must rewrite a queue URL at depth, resolve
    /// must restore it, and a `field.<schema_id>` KEY must be left alone
    /// because `walk_strings_mut` never visits object keys.
    #[test]
    fn nested_query_queue_ref_round_trips_through_portabilize() {
        use crate::snapshot::refs::{portabilize_value, resolve_value};
        let mut lockfile = Lockfile::default();
        lockfile.api_base = "https://acme.rossum.app/api/v1".to_string();
        lockfile.upsert(
            "queues",
            "invoices",
            crate::state::ObjectEntry {
                id: 100,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );

        let url = "https://acme.rossum.app/api/v1/queues/100";
        let mut v = json!({
            "queues_filter": [url],
            "query": { "$and": [
                { "queue": { "$in": [url] } },
                { "field.document_id.string": { "$eq": "x" } }
            ] }
        });

        portabilize_value(&mut v, &lockfile);
        assert_eq!(v["queues_filter"][0], json!("rdc://queues/invoices"));
        assert_eq!(
            v["query"]["$and"][0]["queue"]["$in"][0],
            json!("rdc://queues/invoices"),
            "a ref nested three levels into query must portabilize"
        );
        assert!(
            v["query"]["$and"][1].get("field.document_id.string").is_some(),
            "a schema-field id is an object KEY and must be untouched by design"
        );

        resolve_value(&mut v, &lockfile);
        assert_eq!(v["queues_filter"][0], json!(url));
        assert_eq!(v["query"]["$and"][0]["queue"]["$in"][0], json!(url));
    }

    #[test]
    fn filter_keeps_only_shared() {
        let all = vec![mk(1, "public", true), mk(2, "mine", false), mk(3, "also public", true)];
        let kept = retain_shared(all);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|v| v.shared));
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test --lib pull::saved_views`
Expected: FAIL — `process` / `retain_shared` not found.

- [ ] **Step 4: Implement the driver**

Prepend to `src/cli/pull/saved_views.rs`. This mirrors `pull/labels.rs` step for step; the only addition is `retain_shared`.

```rust
use super::common::{
    PullAction, PullCtx, apply_pull_action, decide_pull_action, record_object,
    skip_on_permission_denied,
};
use crate::log::{Action, Log};
use crate::model::SavedView;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

const KIND: &str = "saved_views";

/// Keep only the views rdc manages.
///
/// Split out so it can be unit-tested without a client.
pub(crate) fn retain_shared(views: Vec<SavedView>) -> Vec<SavedView> {
    views.into_iter().filter(|v| v.shared).collect()
}

/// Phase 1: list saved views, keeping only the SHARED ones.
///
/// The filter MUST happen client-side: the server accepts `?shared=true` and
/// then ignores it, returning private views too (verified on the wire). It is
/// also the safety boundary for the whole kind, not a convenience — a private
/// view belongs to one user, its `query` holds that user's own filter values
/// (customer business data), and `created_by` is read-only so rdc could never
/// restore one to its owner. See the design doc, section B.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<Log>) -> Result<Vec<SavedView>> {
    let all = skip_on_permission_denied(
        ctx.client
            .list_saved_views(Some(progress.clone()))
            .await
            .context("listing saved views"),
        KIND,
        progress,
    )?;
    let total = all.len();
    let shared = retain_shared(all);
    let dropped = total - shared.len();
    if dropped > 0 {
        progress.event(
            Action::Skip,
            &format!("saved_views ({dropped} private — rdc manages shared views only)"),
        );
    }
    Ok(shared)
}

/// Phase 2: write listed saved views to disk. `subset` selects which
/// `(kind, slug)` pairs are actually written. Returns `(count, conflicts)`.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    views: Vec<SavedView>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for v in &views {
        let slug = match ctx.lockfile.slug_for_id(KIND, v.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&v.name, &used),
        };
        used.insert(slug.clone());

        if !subset.contains(&(KIND.to_string(), slug.clone())) {
            continue;
        }

        let result: Result<()> = (|| {
            if !dir_created {
                std::fs::create_dir_all(ctx.paths.saved_views_dir()).with_context(|| {
                    format!("creating {}", ctx.paths.saved_views_dir().display())
                })?;
                dir_created = true;
            }

            let value = serde_json::to_value(v)?;
            let art = crate::snapshot::codec::codec(KIND)
                .unwrap()
                .disk_bytes(&value)
                .context("serializing saved view")?;
            let proposed = art.json;

            let local_path = ctx.paths.saved_views_dir().join(format!("{slug}.json"));
            let base_hash = ctx
                .lockfile
                .objects
                .get(KIND)
                .and_then(|m| m.get(&slug))
                .and_then(|e| e.content_hash.clone());

            let proposed =
                crate::cli::pull::common::portabilize_proposed(&proposed, &*ctx.lockfile);
            let (action, remote_hash) =
                decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
            if action == PullAction::Conflict {
                conflicts += 1;
            }
            let recorded_hash = apply_pull_action(
                action,
                &local_path,
                &proposed,
                remote_hash,
                ctx.interactive,
                progress,
                ctx.paths.env(),
                base_hash.as_deref(),
                Some(ctx.paths),
            )?;

            record_object(
                ctx.lockfile,
                KIND,
                &slug,
                v.id,
                v.modified_at().map(|s| s.to_string()),
                v.modified_by().map(|s| s.to_string()),
                Some(recorded_hash),
            );
            written += 1;
            Ok(())
        })();
        result?;
    }

    if written > 0 {
        progress.event(Action::Pull, &format!("saved_views ({written} pulled)"));
    }

    Ok((written, conflicts))
}
```

Add to `src/cli/pull/mod.rs`:

```rust
pub mod saved_views;
```

- [ ] **Step 5: Wire the catalog**

In `src/cli/pull/common.rs` make five edits, each mirroring the `Labels` / `labels` one immediately beside it:

1. `RemoteCatalog`: `pub saved_views: Vec<crate::model::SavedView>,`
2. The private `Listed` enum: `SavedViews(Vec<crate::model::SavedView>),`
3. The private `Kind` enum: `SavedViews,`
4. The `kinds` array: `Kind::SavedViews,`
5. The dispatch `match` arm, modelled on the labels arm:

```rust
                    Kind::SavedViews => {
                        let r = crate::cli::pull::saved_views::list(ctx_ref, progress).await;
                        r.map(Listed::SavedViews)
                    }
```

6. The local accumulator and its unwrap: add `let mut saved_views: Option<Vec<crate::model::SavedView>> = None;` beside the `labels` one, a `Listed::SavedViews(v) => saved_views = Some(v),` arm, and `saved_views: saved_views.unwrap_or_default(),` in the `RemoteCatalog { … }` construction.

Find the exact sites with:

Run: `grep -n "Labels\|labels" src/cli/pull/common.rs`

- [ ] **Step 6: Wire the portabilize post-pass**

In `src/cli/pull/portabilize.rs`, add the arm to `locate_json_path`:

```rust
        "saved_views" => Some(paths.saved_views_dir().join(format!("{slug}.json"))),
```

Find the function with: `grep -n "fn locate_json_path" -A 25 src/cli/pull/portabilize.rs`

- [ ] **Step 7: Run the tests**

Run: `cargo test --lib pull::saved_views`
Expected: PASS (3 tests).

Run: `cargo test --lib pull::`
Expected: PASS — the catalog change must not break existing pull tests. Any `RemoteCatalog { … }` literal in a test that now misses the field will fail to compile; add `saved_views: vec![],` to each.

Run: `cargo test --test cli_sync`
Expected: PASS. rdc now really lists `/saved_views`, so this is where Step 1's
registration proves itself. A 404 here means an array was missed.

- [ ] **Step 8: Commit**

```bash
git add src/cli/pull/saved_views.rs src/cli/pull/mod.rs src/cli/pull/common.rs src/cli/pull/portabilize.rs tests/cli_sync.rs tests/cli_doctor.rs
git commit -m "$(cat <<'MSG'
feat(saved-views): pull driver with the shared-only filter

The filter has to be client-side: the server accepts ?shared=true and then
ignores it, returning private views anyway (verified on the wire, same hazard as
the silently-ignored ?offset=).

It is the safety boundary for the whole kind rather than a convenience. Without
it every user's private dashboard filters land in the customer's git repo, and
the query field holds filter VALUES -- real business data. created_by is also
read-only, so a private view rdc deleted and re-pushed would come back owned by
the pushing token and be invisible to its creator.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 5: Push driver, scan, tombstones and deletes

**Files:**
- Create: `src/cli/push/saved_views.rs`
- Modify: `src/cli/push/mod.rs` (tally the new driver)
- Modify: `src/cli/push/scan.rs` (`ChangeList` field + `total` + `json_parse_errors` + `field_limit_violations`; `Tombstones` field + `total`; `scan`; `detect_tombstones`; the `unshared_saved_views` method)
- Modify: `src/cli/push/deletes.rs` (`DeleteCounts` field, `reverse_dep_order_iter`, `apply_outcome`, and the two per-kind drift-fetch arms)
- Modify: `src/cli/sync/mod.rs` (surface the new refusal in `refuse_on_offline_defects`)
- Modify: `src/cli/doctor/mod.rs` (only if the doctor tuple needs the new list — check whether it compiles first)

**Interfaces:**
- Consumes: everything from Tasks 1–4.
- Produces: `push::saved_views::push(&Paths, &RossumClient, &mut Lockfile, bool, &BTreeMap<String, PathBuf>, &Arc<Log>, &str) -> Result<(usize, usize)>`; `ChangeList.saved_views`, `Tombstones.saved_views`, `ChangeList::unshared_saved_views() -> Vec<UnsharedSavedView>`. Task 6 inserts into both maps.

- [ ] **Step 1: Write the failing tests**

Append to `src/cli/push/scan.rs`'s test module:

```rust
    #[test]
    fn scan_finds_a_new_saved_view_as_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.saved_views_dir()).unwrap();
        std::fs::write(
            paths.saved_views_dir().join("awaiting-approval.json"),
            br#"{"name":"Awaiting approval","shared":true,"query":{"$and":[]}}"#,
        )
        .unwrap();

        let lockfile = Lockfile::default();
        let (_n, changes, tombstones) = scan(&paths, &lockfile).unwrap();

        assert!(changes.saved_views.contains_key("awaiting-approval"));
        assert!(tombstones.saved_views.is_empty());
    }

    #[test]
    fn a_missing_saved_view_file_is_a_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "saved_views",
            "gone",
            crate::state::ObjectEntry {
                id: 77,
                modified_at: None,
                modified_by: None,
                content_hash: Some("h".into()),
                secrets_hash: None,
            },
        );

        let t = detect_tombstones(&paths, &lockfile);
        assert_eq!(t.saved_views.get("gone"), Some(&77));
    }

    #[test]
    fn unshared_saved_view_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.saved_views_dir()).unwrap();
        let path = paths.saved_views_dir().join("mine.json");
        std::fs::write(&path, br#"{"name":"Mine","shared":false,"query":{"$and":[]}}"#).unwrap();

        let mut changes = ChangeList::default();
        changes.saved_views.insert("mine".to_string(), path.clone());

        let refused = changes.unshared_saved_views();
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].slug, "mine");
    }

    #[test]
    fn shared_saved_view_is_not_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.saved_views_dir()).unwrap();
        let path = paths.saved_views_dir().join("ok.json");
        std::fs::write(&path, br#"{"name":"Ok","shared":true,"query":{"$and":[]}}"#).unwrap();

        let mut changes = ChangeList::default();
        changes.saved_views.insert("ok".to_string(), path);
        assert!(changes.unshared_saved_views().is_empty());
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib push::scan::tests::saved_view` and `cargo test --lib push::scan::tests::unshared`
Expected: FAIL — no `saved_views` field on `ChangeList` / `Tombstones`, no `unshared_saved_views`.

- [ ] **Step 3: Wire `scan.rs`**

Add `pub saved_views: BTreeMap<String, std::path::PathBuf>,` to `ChangeList` and `pub saved_views: BTreeMap<String, u64>,` to `Tombstones`. Add `+ self.saved_views.len()` to both `total()` implementations. Add `check("saved_views", &self.saved_views);` to `json_parse_errors` and to `field_limit_violations`'s sweep list.

In `scan()`, beside the labels call:

```rust
    scanned += scan_flat_kind(
        paths,
        lockfile,
        "saved_views",
        paths.saved_views_dir(),
        &mut changes.saved_views,
    )?;
```

In `detect_tombstones()`, beside the labels line:

```rust
    detect_flat(lockfile, "saved_views", &paths.saved_views_dir(), &mut t.saved_views);
```

Add the method to `impl ChangeList`:

```rust
    /// Local saved-view files rdc refuses to push because they are not shared.
    ///
    /// See `snapshot::limits::check_saved_view_shared` for why this is refused
    /// offline rather than pushed and reconciled.
    pub fn unshared_saved_views(&self) -> Vec<crate::snapshot::limits::UnsharedSavedView> {
        let mut out = Vec::new();
        for (slug, path) in &self.saved_views {
            let Ok(bytes) = std::fs::read(path) else {
                continue; // unreadable != unshared; push surfaces I/O errors
            };
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue; // unparseable is already reported by json_parse_errors
            };
            if !crate::snapshot::limits::check_saved_view_shared(&v) {
                out.push(crate::snapshot::limits::UnsharedSavedView {
                    slug: slug.clone(),
                    path: path.clone(),
                });
            }
        }
        out
    }
```

- [ ] **Step 4: Run the scan tests**

Run: `cargo test --lib push::scan`
Expected: PASS.

- [ ] **Step 5: Write the push driver**

Create `src/cli/push/saved_views.rs` by copying `src/cli/push/labels.rs` and substituting throughout: `labels` → `saved_views`, `Label` → `SavedView`, `label` → `saved view` in prose, `create_label` → `create_saved_view`, `update_label` → `update_saved_view`, `list_labels` → `list_saved_views`, `paths.labels_dir()` → `paths.saved_views_dir()`, and the progress labels `label/{slug}` → `saved_view/{slug}`.

Then make the one behavioural change — strict ref resolution. Where the copied create path calls:

```rust
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
```

replace it with:

```rust
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
            // Saved views do NOT participate in deferred relink. Dropping
            // `queues_filter` to `[]` would silently widen a view scoped to a
            // few queues into one visible to the WHOLE organization (that is
            // the API's own semantics for an empty filter), and `query` is
            // required on POST so deferring it fails the create outright. So an
            // unresolved ref stops the push with a message naming it, rather
            // than sending a body that is quietly wrong.
            let residual = crate::snapshot::refs::residual_rdc_refs(&payload);
            if !residual.is_empty() {
                anyhow::bail!(
                    "saved view '{slug}' references objects that do not exist in this env: {}. \
                     Create them first, or override `query`/`queues_filter` for this env in \
                     overlay.toml.",
                    residual.join(", ")
                );
            }
```

Apply the identical guard on the update path, immediately after its `resolve_value` call.

Add to `src/cli/push/mod.rs`, beside the labels tally:

```rust
        tally(saved_views::push(paths, client, lockfile, interactive, &changes.saved_views, progress, env).await
```

matching the exact surrounding syntax, and add `mod saved_views;` / `use` as the file's convention requires.

- [ ] **Step 6: Wire `deletes.rs`**

Add `pub saved_views: usize,` to `DeleteCounts`. Add `("saved_views", &t.saved_views),` to `reverse_dep_order_iter` — position it beside `("labels", …)`; nothing references a saved view, so it has no ordering constraint. Add `"saved_views" => counts.saved_views += 1,` to `apply_outcome`. Add the two drift-fetch arms beside the labels ones:

```rust
        "saved_views" => client
            .list_saved_views(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| x.modified_at().map(|s| s.to_string())),
```

and

```rust
        "saved_views" => client
            .list_saved_views(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| serde_json::to_value(&x))
            .transpose()?,
```

The DELETE itself needs no change — `delete_path("/{kind}/{id}")` already resolves to `/saved_views/{id}`.

- [ ] **Step 7: Surface the refusal**

In `src/cli/sync/mod.rs` add beside the existing pre-flight calls:

```rust
    let unshared_views = changes.unshared_saved_views();
```

and pass it as a new final parameter to `refuse_on_offline_defects`, extending that function to print one line per entry:

```rust
    for v in unshared_views {
        eprintln!(
            "  - saved-views/{}: `shared` is not true. rdc manages shared saved views only.",
            v.slug
        );
    }
```

Follow the exact reporting shape the function already uses for the other four classes, and include the new class in its "refuse" decision.

- [ ] **Step 8: Run the tests**

Run: `cargo test --lib push::`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src/cli/push/saved_views.rs src/cli/push/mod.rs src/cli/push/scan.rs src/cli/push/deletes.rs src/cli/sync/mod.rs
git commit -m "$(cat <<'MSG'
feat(saved-views): push driver, scan, tombstones and deletes

Modelled on push::labels, with one deliberate difference: saved views do not
participate in deferred relink. An unresolved rdc:// ref stops the push instead
of being deferred, because deferral is unsafe for this kind in two ways --
dropping queues_filter to [] widens a view scoped to a few queues into one
visible to the whole organization (the API's own semantics for an empty filter),
and query is required on POST so deferring it fails the create outright.

The DELETE needs no new client method: delete_path("/{kind}/{id}") already
resolves to /saved_views/{id}.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 6: Sync wiring — classifier and executor

**Files:**
- Modify: `src/cli/sync/mod.rs` (remote-hash block, scan-changes, tombstones, locked)
- Modify: `src/cli/sync/execute.rs` (`saved_view_by_slug` index, `ConflictRefs` arm, `RemoteDeleteRefs` arm, tombstone arm, change-list arm, pull-subset dispatch)

**Interfaces:**
- Consumes: Tasks 1–5.
- Produces: full `rdc sync` participation for the kind. Task 8 relies on the executor arms existing.

- [ ] **Step 1: Write the failing test**

Append to `src/cli/sync/execute.rs`'s test module, modelled on the existing labels tests:

```rust
    /// The clean-`RemoteDelete` event line must not claim a remote deletion for
    /// this kind: a saved view that left rdc's filtered listing has usually
    /// just been unshared, and still exists in the org.
    #[test]
    fn remote_delete_detail_is_kind_specific_for_saved_views() {
        assert_eq!(
            remote_delete_detail("saved_views", "awaiting-approval"),
            "saved_views/awaiting-approval (no longer shared \u{2014} not managed by rdc)"
        );
        assert_eq!(remote_delete_detail("labels", "urgent"), "labels/urgent");
    }
```

Do NOT add an unused `SavedView` test-fixture helper here (ruling R3): Task 10
gates on `cargo clippy --all-targets -- -D warnings`, and an unused helper is a
`dead_code` warning that would fail that gate. Task 9's integration coverage
lives in a separate test binary and builds its fixtures inline.


- [ ] **Step 2: Run it**

Run: `cargo test --lib sync::execute::tests::saved_view`
Expected: FAIL to compile until the wiring below exists.

- [ ] **Step 3: Wire `sync/mod.rs`**

Copy the whole `// --- labels ---` block and adapt it. Four insertions:

```rust
    // --- saved views ---------------------------------------------------
    let saved_views_codec =
        crate::snapshot::codec::codec("saved_views").expect("saved_views codec must exist");
    let mut used_saved_view_slugs: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for v in &catalog.saved_views {
        let slug = match lockfile.slug_for_id("saved_views", v.id) {
            Some(existing) => existing.to_string(),
            None => crate::slug::slugify_unique(&v.name, &used_saved_view_slugs),
        };
        used_saved_view_slugs.insert(slug.clone());

        let value = match serde_json::to_value(v) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let art = match saved_views_codec.disk_bytes(&value) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let json = crate::cli::pull::common::portabilize_proposed(&art.json, lockfile);
        let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
        remote_hashes.insert(("saved_views".to_string(), slug), hash);
    }

    for (slug, path) in &changes.saved_views {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let hash = crate::state::content_hash(&bytes, &crate::state::Lockfile::default());
        scan_changes.insert(("saved_views".to_string(), slug.clone()), hash);
    }

    for slug in tombstones.saved_views.keys() {
        scan_tombstones.insert(("saved_views".to_string(), slug.clone()));
    }

    if let Some(map) = lockfile.objects.get("saved_views") {
        for (slug, entry) in map {
            if let Some(h) = &entry.content_hash {
                locked.insert(("saved_views".to_string(), slug.clone()), h.clone());
            }
        }
    }
```

- [ ] **Step 4: Wire `sync/execute.rs`**

Five insertions, each modelled on the labels one beside it.

1. The slug index, beside `label_by_slug`:

```rust
    let mut saved_view_by_slug: BTreeMap<String, &crate::model::SavedView> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for v in &catalog.saved_views {
            let slug = match ctx.lockfile.slug_for_id("saved_views", v.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&v.name, &used),
            };
            used.insert(slug.clone());
            saved_view_by_slug.insert(slug, v);
        }
    }
```

2. The `ConflictRefs` arm:

```rust
            "saved_views" => saved_view_by_slug.get(it.slug.as_str()).copied().and_then(|v| {
                let codec = crate::snapshot::codec::codec("saved_views")?;
                let value = serde_json::to_value(v).ok()?;
                let art = codec.disk_bytes(&value).ok()?;
                let local_path = ctx.paths.saved_views_dir().join(format!("{}.json", it.slug));
                Some(ConflictRefs {
                    remote_bytes: art.json,
                    remote_code: None,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: v.id,
                    modified_at: v.modified_at().map(|s| s.to_string()),
                    modified_by: v.modified_by().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Flat,
                })
            }),
```

Match the surrounding arm's exact field list — copy the labels arm and edit rather than trusting this verbatim.

3. The `RemoteDeleteRefs` arm, same shape as the labels one, using `saved_view_by_slug` and `ctx.paths.saved_views_dir()`.

4. The tombstone arm:

```rust
                "saved_views" => {
                    tombstones.saved_views.insert(it.slug.clone(), id);
                }
```

5. The promoted-to-push arm:

```rust
                "saved_views" => {
                    change_list.saved_views.insert(slug, path);
                }
```

6. The pull-subset dispatch, beside the labels one:

```rust
        if let Some(subset) = subsets.get("saved_views") {
            crate::cli::pull::saved_views::process(
                ctx,
                catalog.saved_views.clone(),
                subset,
                progress,
            )
            .await?;
        }
```

- [ ] **Step 5: Reword the remote-delete message for this kind**

Add the helper near the other free functions in `src/cli/sync/execute.rs`:

```rust
/// Event detail for a clean `RemoteDelete`.
///
/// For most kinds the remote object really is gone. A saved view usually is
/// not: it left rdc's filtered listing because someone unshared it, and it
/// still exists in the organization. Claiming a remote deletion there would be
/// false, so the wording is kind-specific.
fn remote_delete_detail(kind: &str, slug: &str) -> String {
    if kind == "saved_views" {
        format!("saved_views/{slug} (no longer shared \u{2014} not managed by rdc)")
    } else {
        format!("{kind}/{slug}")
    }
}
```

Then find where the clean-`RemoteDelete` event line is emitted
(`grep -n "Action::Delete" src/cli/sync/execute.rs`) and route it through the
helper: `progress.event(Action::Delete, &remote_delete_detail(&it.kind, &it.slug));`

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib sync::`
Expected: PASS. Add `saved_views: vec![],` to every `RemoteCatalog { … }` literal in tests that now fails to compile.

- [ ] **Step 7: Commit**

```bash
git add src/cli/sync/mod.rs src/cli/sync/execute.rs
git commit -m "$(cat <<'MSG'
feat(saved-views): wire the kind into the sync classifier and executor

A shared view flipped to private in the UI leaves rdc's filtered listing and is
classified as a clean RemoteDelete, which drops the local file and the lockfile
entry together -- so no tombstone follows and the remote view is never touched.
Re-sharing brings the file back as a RemoteCreate.

The event wording is kind-specific because the object usually still exists:
"no longer shared -- not managed by rdc" rather than a remote-deletion claim
that would be false.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 7: Mapping, realign and the index emitter

Pure registry extension. Every array here is fixed-size, so a missed site is a compile error rather than a silent gap.

**Files:**
- Modify: `src/mapping.rs` (`Mapping` field + `Default` + `kind_map` + `kind_map_mut`; `GenericMapping` field + `Default` + `KINDS` 10→11 + `kind_rows` + `kind_rows_mut`)
- Modify: `src/overlay.rs` (`kind_maps()` return array 9→10)
- Modify: `src/cli/deploy/realign.rs` (`PendingRename::SavedView`, the `detect_flat_kind` call, the three other `match` sites, `OVERLAY_KINDS` 9→10)
- Modify: `src/cli/index.rs` (`RICH_KINDS` + `emit_saved_views` + its call site)

**Interfaces:**
- Consumes: Tasks 1–6.
- Produces: `Mapping.saved_views`, `GenericMapping.saved_views`, `PendingRename::SavedView { old, new }`. Task 8 uses `Mapping::kind_map("saved_views")`.

- [ ] **Step 1: Write the failing tests**

Append to `src/mapping.rs`'s tests:

```rust
    #[test]
    fn saved_views_is_a_mapping_kind() {
        assert!(GenericMapping::KINDS.contains(&"saved_views"));
        let g = GenericMapping::default();
        assert!(g.kind_rows("saved_views").is_some());
        let m = Mapping::default();
        assert!(m.kind_map("saved_views").is_some());
    }

    #[test]
    fn a_mapping_toml_without_saved_views_still_parses() {
        // Backward compatibility: an existing project's mapping file predates
        // the kind and must load unchanged.
        let g: GenericMapping = toml::from_str("version = 2\n").unwrap();
        assert!(g.kind_rows("saved_views").unwrap().is_empty());
    }
```

Append to `src/overlay.rs`'s tests:

```rust
    #[test]
    fn kind_maps_includes_saved_views() {
        let o = Overlay::default();
        assert!(o.kind_maps().iter().any(|(k, _)| *k == "saved_views"));
    }

    #[test]
    fn an_overlay_without_saved_views_still_parses() {
        let o: Overlay = toml::from_str("version = 1\n").unwrap();
        assert!(o.saved_views.is_empty());
    }
```

- [ ] **Step 2: Run them**

Run: `cargo test --lib mapping` and `cargo test --lib overlay`
Expected: FAIL.

- [ ] **Step 3: Implement the mapping changes**

In `Mapping`, beside `labels`:

```rust
    /// Saved-view slug → saved-view slug.
    #[serde(default)]
    pub saved_views: BTreeMap<String, String>,
```

Add `saved_views: BTreeMap::new(),` to `impl Default for Mapping`, and `"saved_views" => &self.saved_views,` / `"saved_views" => &mut self.saved_views,` to `kind_map` / `kind_map_mut`.

In `GenericMapping`:

```rust
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub saved_views: Vec<BTreeMap<String, String>>,
```

Add `saved_views: Vec::new(),` to its `Default`, add `"saved_views"` to `KINDS` and change the array length to 11, and add the two `kind_rows` arms.

Check whether `GenericMapping::orient` enumerates kinds explicitly (`grep -n "fn orient" -A 30 src/mapping.rs`); if it does, add `saved_views` there too.

- [ ] **Step 4: Implement the overlay and realign changes**

In `src/overlay.rs`, change `kind_maps()`'s return type to `[…; 10]` and add `("saved_views", &self.saved_views),`.

In `src/cli/deploy/realign.rs`:

```rust
    SavedView {
        old: String,
        new: String,
    },
```

added to `PendingRename`; the detection call beside the labels one:

```rust
    detect_flat_kind(lockfile, "saved_views", paths.saved_views_dir(), &mut out, |o, n| {
        PendingRename::SavedView { old: o, new: n }
    });
```

and an arm in each of the three other `match`es over `PendingRename` — find them with `grep -n "PendingRename::Label" src/cli/deploy/realign.rs` and mirror each:

```rust
        PendingRename::SavedView { old, new } => vec![pair("saved_views", old, new)],
```
```rust
        PendingRename::SavedView { old, new } => vec![whole("saved_views", old, new)],
```
and in the rename applier, `rename_lockfile_key(lockfile, "saved_views", old, new);` plus `collect_orphans(paths, "saved_views", old, &mut orphans);`.

Extend `OVERLAY_KINDS` to `[&str; 10]` with `"saved_views"`.

- [ ] **Step 5: Implement the index emitter**

Add `"saved_views"` to `RICH_KINDS` in `src/cli/index.rs`, and the emitter beside `emit_labels`:

```rust
fn emit_saved_views(md: &mut String, ctx: &IndexCtx<'_>) {
    let Some(entries) = ctx.lockfile.objects.get("saved_views") else {
        return;
    };
    if entries.is_empty() {
        return;
    }
    md.push_str("## saved views\n\n");
    for (slug, entry) in entries.iter() {
        let path = ctx.paths.saved_views_dir().join(format!("{slug}.json"));
        let v = read_json(&path);
        write_header(md, slug, entry.id);
        write_name(md, v.as_ref());
        md.push_str(&format!("  - path: saved-views/{slug}.json\n"));
    }
    md.push('\n');
}
```

Call it wherever `emit_labels` is called.

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib mapping` `cargo test --lib overlay` `cargo test --lib realign` `cargo test --lib index`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/mapping.rs src/overlay.rs src/cli/deploy/realign.rs src/cli/index.rs
git commit -m "$(cat <<'MSG'
feat(saved-views): register the kind with mapping, realign and the index

Every list touched here is a fixed-size array, so a missed site is a compile
error rather than a silent gap.

Both mapping.toml and overlay.toml stay backward compatible: every field is
#[serde(default)] and neither struct uses deny_unknown_fields, so an existing
project's files load unchanged and a newer file still loads on an older binary
with the section ignored. Tests pin both directions.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 8: Migrate wiring and the cross-env ref validation

The migrate half, plus the two hard errors that replace silently-wrong promotion.

**Files:**
- Modify: `src/cli/migrate/mod.rs` (`classify`, `MANAGED_DIRS`, the overlay dispatch, `remap_relative`, `SUBST_KINDS` decision, and the new validator)
- Modify: `src/cli/deploy/selection.rs` (`DEPLOYABLE_KINDS`)

**Interfaces:**
- Consumes: Tasks 1–7.
- Produces: `pub(crate) struct SavedViewRefProblem { pub slug: String, pub location: String, pub reference: String, pub reason: SavedViewRefReason }`, `pub(crate) enum SavedViewRefReason { NonPortable, Unresolvable }`, and `pub(crate) fn check_saved_view_refs(slug: &str, value: &Value, known: &BTreeSet<(String, String)>) -> Vec<SavedViewRefProblem>`.

- [ ] **Step 1: Write the failing tests**

Append to `src/cli/migrate/mod.rs`'s tests:

```rust
    fn known_pairs(pairs: &[(&str, &str)]) -> BTreeSet<(String, String)> {
        pairs.iter().map(|(k, s)| (k.to_string(), s.to_string())).collect()
    }

    #[test]
    fn saved_view_with_only_resolvable_refs_is_clean() {
        let v = serde_json::json!({
            "name": "Awaiting approval",
            "shared": true,
            "queues_filter": ["rdc://queues/invoices"],
            "query": { "$and": [ { "queue": { "$in": ["rdc://queues/invoices"] } } ] }
        });
        let known = known_pairs(&[("queues", "invoices")]);
        assert!(check_saved_view_refs("awaiting-approval", &v, &known).is_empty());
    }

    /// A user ref survives portabilization as a raw URL because users are not a
    /// snapshotted kind. Promoting it would 400 -- the server validates refs
    /// inside `query` as hyperlinks -- so migrate refuses instead.
    #[test]
    fn saved_view_with_a_non_portable_user_ref_is_refused() {
        let v = serde_json::json!({
            "name": "Mine",
            "shared": true,
            "queues_filter": [],
            "query": { "$and": [
                { "modifier": { "$in": ["https://acme.rossum.app/api/v1/users/7"] } }
            ] }
        });
        let problems = check_saved_view_refs("mine", &v, &BTreeSet::new());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].reason, SavedViewRefReason::NonPortable);
        assert!(problems[0].reference.contains("/users/7"));
        assert!(
            problems[0].location.contains("query"),
            "the location must point into query, got {}",
            problems[0].location
        );
    }

    /// An unresolvable queues_filter ref must NOT be deferred: an empty
    /// queues_filter makes a shared view visible to the entire organization.
    #[test]
    fn saved_view_with_an_unresolvable_queue_ref_is_refused() {
        let v = serde_json::json!({
            "name": "Scoped",
            "shared": true,
            "queues_filter": ["rdc://queues/not-in-target"],
            "query": { "$and": [] }
        });
        let problems = check_saved_view_refs("scoped", &v, &known_pairs(&[("queues", "invoices")]));
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].reason, SavedViewRefReason::Unresolvable);
        assert_eq!(problems[0].location, "queues_filter[0]");
        assert!(problems[0].reference.contains("not-in-target"));
    }

    /// `field.<schema_id>` keys are object KEYS, not string leaves. They are
    /// deliberately NOT checked -- documented in the design doc, not guarded.
    #[test]
    fn schema_field_keys_are_not_checked() {
        let v = serde_json::json!({
            "name": "By field",
            "shared": true,
            "queues_filter": [],
            "query": { "$and": [ { "field.document_id.string": { "$eq": "x" } } ] }
        });
        assert!(check_saved_view_refs("by-field", &v, &BTreeSet::new()).is_empty());
    }
```

- [ ] **Step 2: Run them**

Run: `cargo test --lib migrate::tests::saved_view`
Expected: FAIL — `check_saved_view_refs` not found.

- [ ] **Step 3: Implement the validator**

Add to `src/cli/migrate/mod.rs`:

```rust
/// Why a saved view's reference cannot cross into the target env.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SavedViewRefReason {
    /// A raw API URL that survived portabilization — it names a kind rdc does
    /// not snapshot (users are the common case), so there is nothing to remap
    /// it to. The server validates refs inside `query` as hyperlinks, so
    /// promoting it would 400.
    NonPortable,
    /// A well-formed `rdc://<kind>/<slug>` naming an object the target snapshot
    /// does not contain.
    Unresolvable,
}

/// One reference in a saved view that blocks promotion.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SavedViewRefProblem {
    pub slug: String,
    /// A JSON path into the view body, e.g. `queues_filter[0]` or
    /// `query.$and[1].modifier.$in[0]`.
    pub location: String,
    pub reference: String,
    pub reason: SavedViewRefReason,
}

/// Walk every string leaf of `value`, tracking a JSON path.
///
/// `walk_strings_mut` in `snapshot::refs` deliberately carries no path (it is a
/// blind rewriter), and the error messages here are only useful if they can say
/// WHERE the bad ref is — so this is a separate, path-aware walk. Object keys
/// are not visited, which is exactly why `field.<schema_id>` keys are out of
/// scope.
fn walk_strings_with_path(value: &Value, path: &str, f: &mut dyn FnMut(&str, &str)) {
    match value {
        Value::String(s) => f(path, s),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk_strings_with_path(item, &format!("{path}[{i}]"), f);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                let child = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                walk_strings_with_path(v, &child, f);
            }
        }
        _ => {}
    }
}

/// Validate a migrated saved view's references against what the target snapshot
/// actually contains.
///
/// `known` is the set of `(kind, slug)` pairs the migration produced, derived by
/// running [`classify`] over the enumerated target files.
///
/// Saved views are checked strictly rather than deferred. `resolve_value_deferring`
/// would drop a top-level field still holding `rdc://` refs and PATCH it later,
/// which for this kind is unsafe twice over: an empty `queues_filter` makes a
/// shared view visible to the WHOLE organization, and `query` is required on
/// POST so deferring it fails the create.
pub(crate) fn check_saved_view_refs(
    slug: &str,
    value: &Value,
    known: &BTreeSet<(String, String)>,
) -> Vec<SavedViewRefProblem> {
    let mut out = Vec::new();
    walk_strings_with_path(value, "", &mut |location, s| {
        if let Some((kind, target)) = crate::snapshot::refs::parse_rdc_ref(s) {
            if !known.contains(&(kind.to_string(), target.to_string())) {
                out.push(SavedViewRefProblem {
                    slug: slug.to_string(),
                    location: location.to_string(),
                    reference: s.to_string(),
                    reason: SavedViewRefReason::Unresolvable,
                });
            }
        } else if s.contains("/api/v1/") && s.starts_with("http") {
            out.push(SavedViewRefProblem {
                slug: slug.to_string(),
                location: location.to_string(),
                reference: s.to_string(),
                reason: SavedViewRefReason::NonPortable,
            });
        }
    });
    out.sort_by(|a, b| a.location.cmp(&b.location));
    out
}
```

Ensure `BTreeSet` and `Value` are imported in the module.

- [ ] **Step 4: Run the validator tests**

Run: `cargo test --lib migrate::tests::saved_view`
Expected: PASS (4 tests).

- [ ] **Step 5: Wire migrate**

In `classify`, beside the labels arm:

```rust
        Some("saved-views") if comps.len() == 2 => leaf
            .strip_suffix(".json")
            .map(|s| ("saved_views", s.to_string())),
```

Note the directory is hyphenated while the kind is underscored.

Add `"saved-views"` to `MANAGED_DIRS`. Add `"saved_views" => overlay.saved_view(slug),` to the overlay dispatch. Add to `remap_relative`:

```rust
        "saved-views" if comps.len() == 2 => remap_flat_leaf(&comps, "saved_views", mapping),
```

Check `remap_flat_leaf`'s signature — it likely derives the directory name from the kind string. If it does, add a variant or pass the directory explicitly so `saved_views` maps to `saved-views/`. Verify with `grep -n "fn remap_flat_leaf" -A 15 src/cli/migrate/mod.rs` and adapt.

Do **not** add `saved_views` to `SUBST_KINDS`: nothing references a saved view, so it is never a substitution target — the same reason `engine_fields` and `email_templates` are absent.

Add `"saved_views"` to `DEPLOYABLE_KINDS` in `src/cli/deploy/selection.rs`, positioned **after** `"queues"` so its refs resolve. Beside `"labels"` satisfies that.

- [ ] **Step 6: Call the validator from the migrate run**

After the target files are written and enumerated, build `known` and refuse:

```rust
    // Saved views resolve strictly — see `check_saved_view_refs`.
    let known: BTreeSet<(String, String)> = tgt_files
        .iter()
        .filter_map(|rel| classify(rel).map(|(k, s)| (k.to_string(), s)))
        .collect();
    let mut ref_problems: Vec<SavedViewRefProblem> = Vec::new();
    for rel in &tgt_files {
        let Some(("saved_views", slug)) = classify(rel) else {
            continue;
        };
        // An overlay that replaces `query` for this env is the documented
        // escape hatch, and the overlay has already been applied to the file on
        // disk, so reading it back covers that case for free.
        let path = tgt_paths.env_root().join(rel);
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { continue };
        ref_problems.extend(check_saved_view_refs(&slug, &v, &known));
    }
    if !ref_problems.is_empty() {
        for p in &ref_problems {
            let why = match p.reason {
                SavedViewRefReason::NonPortable => {
                    "not a portable reference (it names a kind rdc does not manage, e.g. a user)"
                }
                SavedViewRefReason::Unresolvable => "no such object in the target snapshot",
            };
            eprintln!("  - saved-views/{}: {} → {} ({why})", p.slug, p.location, p.reference);
        }
        anyhow::bail!(
            "{} saved-view reference(s) cannot cross into '{}'. \
             Fix the source view, or override `query` / `queues_filter` for this env in \
             overlay.toml. rdc refuses rather than dropping the clause, because an empty \
             queues_filter would make a shared view visible to the whole organization and a \
             dropped query filter would silently change what the view shows.",
            ref_problems.len(),
            tgt_paths.env()
        );
    }
```

Adapt the variable names to the surrounding function (`tgt_files`, `tgt_paths`) — read the enclosing function first and match what it actually has.

- [ ] **Step 7: Run the migrate tests**

Run: `cargo test --lib migrate`
Expected: PASS.

Run: `cargo test --test cli_migrate`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/cli/migrate/mod.rs src/cli/deploy/selection.rs
git commit -m "$(cat <<'MSG'
feat(saved-views): promote across envs, refusing refs that cannot cross

queues_filter and the refs inside query portabilize for free -- they are URLs,
and the walker reaches every string leaf at any depth. What needed new code is
the refusal.

Two hard errors replace silently-wrong promotion. A raw API URL that survived
portabilization names a kind rdc does not snapshot (users, in practice) and the
server validates refs inside query as hyperlinks, so promoting it would 400. An
rdc:// ref with no counterpart in the target snapshot is refused rather than
deferred, because deferral drops the field: an empty queues_filter makes a
shared view visible to the WHOLE organization, and query is required on POST.

Dropping the clause instead would turn "modified by one person" into
"everything" -- a meaning change dressed up as a cleanup. The documented escape
hatch is an overlay that replaces query for the target env.

field.<schema_id> keys are object keys, which the walker never visits, so they
stay out of scope and are documented in the spec instead.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 9: README and end-to-end integration coverage

**Files:**
- Modify: `README.md` (the snapshot-tree diagram)
- Modify: `tests/cli_sync.rs` (wiremock coverage)

**Interfaces:**
- Consumes: Tasks 1–8.
- Produces: no new code interfaces.

- [ ] **Step 1: Update the README tree**

In the `envs/test/` diagram, add the directory after `labels/`:

```
├── labels/
├── saved-views/
│   └── awaiting-approval.json
```

The kind→files table above it lists only kinds with code sidecars, so saved views need no row there.

- [ ] **Step 2: Confirm Task 4's endpoint registration is still complete**

Task 4 registered `/api/v1/saved_views` in the mock arrays (ruling R2 — it has to
happen there, because Task 4 is what makes rdc list the endpoint on every sync).

**Do NOT compare counts against `email_templates`.** Ruling R4: the arrays that
mention `email_templates` are two structurally different things, and only one of
them wants a `saved_views` entry:

- **mock-all arrays** — the `CORE_LIST_ENDPOINTS` const plus the per-test arrays
  that enumerate every endpoint to be answered with an empty page. These DO need
  `saved_views`.
- **override/skip arguments** — the `&[…]` second argument to
  `mock_empty_lists_except(server, override_paths)`, which names endpoints the
  test mocks *itself* and the helper must therefore NOT mock. Adding
  `saved_views` to one of these tells the helper to leave the endpoint unmocked,
  and the test 404s.

Re-verify by shape, not by count:

Run: `grep -n '"/api/v1/saved_views"' tests/cli_sync.rs tests/cli_doctor.rs`
Expected: 4 hits — `tests/cli_sync.rs` in `CORE_LIST_ENDPOINTS` and in two
mock-all arrays, plus one in `tests/cli_doctor.rs`. Every hit must sit in a
mock-all array, never in a `mock_empty_lists_except(…)` argument list.

Run: `cargo test --test cli_sync`
Expected: PASS — that is the real proof, and it is what caught the original error.

- [ ] **Step 3: Write the failing integration test**

Append to `tests/cli_sync.rs`, modelled on `sync_remote_create_writes_local_label`:

```rust
/// Pull-side RemoteCreate for a saved view, and the shared-only filter.
///
/// The env exposes one shared and one private view. Only the shared one may
/// reach the snapshot — the filter is the safety boundary for this kind, and no
/// mock can prove it any other way because the server ignores `?shared=true`.
#[tokio::test]
async fn sync_pulls_shared_saved_views_and_ignores_private_ones() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let views_body = serde_json::json!({
        "pagination": { "total": 2, "total_pages": 1, "next": null, "previous": null },
        "results": [
            {
                "id": 11,
                "url": format!("{}/api/v1/saved_views/11", server.uri()),
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "name": "Awaiting approval",
                "shared": true,
                "queues_filter": [],
                "query": { "$and": [ { "status": { "$in": ["to_review"] } } ] },
                "created_by": format!("{}/api/v1/users/7", server.uri()),
                "created_at": "2026-08-01T08:00:00Z",
                "modified_at": "2026-08-02T09:00:00Z"
            },
            {
                "id": 12,
                "url": format!("{}/api/v1/saved_views/12", server.uri()),
                "organization": format!("{}/api/v1/organizations/1", server.uri()),
                "name": "Just mine",
                "shared": false,
                "queues_filter": [],
                "query": { "$and": [] },
                "created_by": format!("{}/api/v1/users/8", server.uri()),
                "created_at": "2026-08-01T08:00:00Z",
                "modified_at": "2026-08-02T09:00:00Z"
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/saved_views"))
        .respond_with(ResponseTemplate::new(200).set_body_json(views_body))
        .mount(&server)
        .await;

    mock_empty_lists_except(&server, &["/api/v1/saved_views"]).await;

    let project = TempDir::new().unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    let _cwd_guard = cwd_lock();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    let result = rdc::cli::sync::run(
        "dev", /* interactive = */ false, /* dry_run = */ false,
        /* allow_deletes = */ false, /* no_push = */ false, /* no_pull = */ false,
        None,
    )
    .await;
    std::env::set_current_dir(&prev_cwd).unwrap();
    result.expect("sync should succeed");

    // Pull-side only: no mutations.
    for req in server.received_requests().await.unwrap_or_default() {
        let p = req.url.path();
        if p.contains("/svc/data-storage/") {
            continue;
        }
        assert!(
            !matches!(
                req.method,
                http::Method::POST | http::Method::PATCH | http::Method::DELETE
            ),
            "unexpected mutating request: {} {}",
            req.method,
            p
        );
    }

    let dir = project.path().join("envs/dev/saved-views");
    let shared_path = dir.join("awaiting-approval.json");
    assert!(shared_path.exists(), "shared view must be written");

    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        files,
        vec!["awaiting-approval.json".to_string()],
        "the private view must NOT be snapshotted; got {files:?}"
    );

    // Server-owned fields must not be on disk.
    let body = std::fs::read_to_string(&shared_path).unwrap();
    for gone in ["created_by", "created_at", "modified_at", "modified_by"] {
        assert!(!body.contains(gone), "{gone} must be stripped; got:\n{body}");
    }
    assert!(body.contains("Awaiting approval"), "content: {body}");

    let lf = std::fs::read_to_string(project.path().join(".rdc/state/dev.lock.json")).unwrap();
    assert!(lf.contains("saved_views"), "lockfile must record the kind: {lf}");
    assert!(lf.contains("awaiting-approval"), "lockfile must record the slug: {lf}");
    assert!(
        !lf.contains("just-mine"),
        "the private view must not be in the lockfile: {lf}"
    );
}
```

- [ ] **Step 4: Run it**

Run: `cargo test --test cli_sync sync_pulls_shared_saved_views`
Expected: PASS.

- [ ] **Step 5: Add the unshared-refusal integration test**

```rust
/// A hand-authored view without `shared: true` is refused offline — before the
/// first remote write, so a permanent failure cannot wedge the project.
#[tokio::test]
async fn sync_refuses_an_unshared_saved_view() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;
    mock_empty_lists_except(&server, &[]).await;

    let project = TempDir::new().unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    let dir = project.path().join("envs/dev/saved-views");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mine.json"),
        br#"{"name":"Mine","shared":false,"query":{"$and":[]}}"#,
    )
    .unwrap();

    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("saved-views/mine"))
        .stderr(predicates::str::contains("shared"));

    // Refused BEFORE the first write: no mutating request reached the server.
    for req in server.received_requests().await.unwrap_or_default() {
        assert!(
            !matches!(
                req.method,
                http::Method::POST | http::Method::PATCH | http::Method::DELETE
            ),
            "refusal must precede every remote write; saw {} {}",
            req.method,
            req.url.path()
        );
    }
}
```

If `predicates` is not already imported in this file, use the assertion style the
neighbouring tests use (`String::from_utf8_lossy` over `output.stderr` plus
`assert!(… .contains(…))`) rather than adding a dependency.

- [ ] **Step 6: Run the sync integration suite**

Run: `cargo test --test cli_sync`
Expected: PASS. Do not start any rebuild while this runs.

- [ ] **Step 7: Commit**

```bash
git add README.md tests/cli_sync.rs tests/cli_doctor.rs
git commit -m "$(cat <<'MSG'
test(saved-views): end-to-end sync coverage, and document the directory

Covers the two behaviours that matter most and that unit tests cannot reach: a
private view in the listing never reaches the snapshot, and a hand-authored file
without shared: true is refused before any network write.

Every mock-endpoint array in the test suite now answers /saved_views -- eleven
of them across two files, only one of which is a shared const the compiler
checks. rdc lists the endpoint on every sync, so a missed array is a wiremock
404 and a red test.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

### Task 10: Live scenario, full suite and clippy

**Files:**
- Create: `tests/live/scenarios/saved_views.rs`
- Modify: `tests/live/scenarios/mod.rs` (register the module)

**Interfaces:**
- Consumes: everything.
- Produces: nothing further.

- [ ] **Step 1: Write the live scenario**

Model it on an existing scenario in `tests/live/scenarios/`. It must be `#[ignore]` like its neighbours, and it MUST delete every object it creates, including on the failure path.

```rust
//! Live round-trip for the `saved_views` kind.
//!
//! Creates one SHARED and one PRIVATE view directly on the API, syncs, and
//! asserts the shared one is snapshotted while the private one is not. Deletes
//! both before returning.
```

Cover: create shared + private remotely → `rdc sync` → shared view present on disk with no `created_by`, private view absent → edit the local `name` → sync → remote reflects the edit → delete the local file → `rdc sync --allow-deletes` → remote view is gone (`GET` → 404).

- [ ] **Step 2: Run the live scenario**

```bash
export RDC_LIVE_API_BASE="https://<host>/v1"
export RDC_LIVE_ORG_ID="<org id>"
export RDC_LIVE_TOKEN="<token>"
cargo test --test live saved_views -- --ignored --test-threads=1
```

Expected: PASS. If credentials are absent the scenario skips — that is the designed behaviour, not a pass.

- [ ] **Step 3: Run the whole suite**

Run: `cargo test`
Expected: PASS, no regressions. Investigate every failure — a `RemoteCatalog`, `ChangeList`, `Tombstones` or `DeleteCounts` literal in an old test may simply need the new field.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean. The weekly release workflow gates on this, so a warning here blocks all releases.

Do **not** run `cargo fmt` — this crate is not fmt-clean under current rustfmt and a repo-wide run would produce a huge unrelated diff.

- [ ] **Step 5: Verify idempotency by hand**

Against a real env with at least one shared saved view:

```bash
rdc sync <env>          # first run: pulls the views
rdc sync <env>          # second run: MUST report 0 changes
git status --short      # MUST be clean after the second run
```

A second run that still reports changes means the codec's disk bytes and the classifier's remote hash disagree — the phantom-drift class this codebase has fought repeatedly. Fix before finishing.

- [ ] **Step 6: Commit**

```bash
git add tests/live/scenarios/saved_views.rs tests/live/scenarios/mod.rs
git commit -m "$(cat <<'MSG'
test(saved-views): live round-trip scenario against a real org

Creates a shared and a private view on the API, syncs, and asserts the shared
one is snapshotted while the private one is not -- the one property no mock can
really prove, since the filter exists precisely because the server ignores
?shared=true.

Also covers local edit -> push and local delete -> remote delete. Every object
it creates is deleted before it returns, including on the failure path.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
MSG
)"
```

---

## Done when

- `rdc sync <env>` pulls shared saved views into `envs/<env>/saved-views/`, ignores private ones, and a second consecutive run reports zero changes with a clean `git status`.
- A local edit pushes; deleting a local file plus `--allow-deletes` deletes the remote view.
- A file without `shared: true` is refused offline with a message naming the slug.
- `rdc migrate <src> <tgt>` promotes views, and refuses with a precise message when a ref cannot cross.
- An overlay `query` override for the target env clears that refusal.
- `cargo test` and `cargo clippy --all-targets -- -D warnings` are both clean.
