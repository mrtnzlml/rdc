# Portable References (Stage 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the on-disk snapshot environment-portable by storing internal cross-references as `rdc://<kind>/<slug>` instead of live API URLs — converting URL→`rdc://` as a post-pass on every pull, and resolving `rdc://`→URL just before every push send.

**Architecture:** A new `src/snapshot/refs.rs` module owns the `rdc://` scheme (parse/format, classify portable kinds, and the two directional conversions over a `serde_json::Value`). Pull gains a post-pass that walks every just-written snapshot file, rewrites internal-kind URLs to `rdc://`, and re-records the lockfile `content_hash` so the next sync sees `Clean`. Each push driver resolves `rdc://`→URL on its payload `Value` immediately after reading it from disk (before typed deserialization / send), using the env's own lockfile. The lockfile format is **unchanged** in this stage — conversion reuses the existing `Lockfile::lookup_url` (URL→slug) and `Lockfile::url_for_slug` (slug→URL).

**Tech Stack:** Rust, `serde_json::Value`, existing `KindCodec` + `combined_hash`/`hook_combined_hash`/`rule_combined_hash`/`schema_combined_hash`/`content_hash`, `anyhow`, wiremock + temp-dir integration tests, `cargo test`/`clippy -D warnings`/`fmt`.

---

## Scope & staging note

This is **Stage 1 of 5** of the redesign in `docs/superpowers/specs/2026-06-05-portable-snapshots-migrate-sync-design.md`. The spec's §10 bundles "portable refs + lockfile v3" into S1; this plan **deliberately splits them** for lower risk and smaller, independently-shippable diffs:

- **Stage 1 (this plan):** portable refs in snapshots. Lockfile **unchanged** (still stores `url`); conversion reuses existing lockfile methods. Ships env-portable snapshots; `deploy`/`sync` keep working.
- **Stage 2 (next plan):** lockfile **v3** — reimplement `lookup_url`/`slug_for_url` to parse the URL path (host-agnostic), `url_for_slug` to derive from `id`+`api_base`, drop the persisted `url` field (the ~80-site `ObjectEntry` mechanical change), bump v2→v3. Behavior-preserving.
- **Stage 3:** dependency-ordered `sync` push + delete `rewrite_urls`.
- **Stage 4:** `rdc migrate` command + unified mapping v2 (`[refs]` `rdc://`→`rdc://` dict).
- **Stage 5:** remove `rdc deploy` (guiding error), relocate store_extensions/hook_secrets/mdh.

**Why Stage 1 is safe & atomic:** pull-converts-but-push-doesn't would send `rdc://` to the API (broken). So pull conversion and push resolution **must land together** — they are one shippable unit, which is exactly this plan.

---

## File Structure

- **Create:** `src/snapshot/refs.rs` — the `rdc://` scheme: `walk_strings_mut`, `parse_rdc_ref`, `is_portable_kind`, `url_to_rdc`, `rdc_to_url`, `portabilize_value`, `resolve_value`. One clear responsibility: convert references between URL and `rdc://` form over a JSON `Value`.
- **Modify:** `src/snapshot/mod.rs` — add `pub mod refs;`.
- **Modify:** `src/snapshot/noise.rs:164` — `is_url` also recognizes `rdc://` (so `sort_url_arrays` keeps ref-arrays order-insensitive after conversion).
- **Modify:** `src/cli/deploy/common.rs:275` — delete the local `walk_strings_mut`; use `crate::snapshot::refs::walk_strings_mut`.
- **Create:** `src/cli/pull/portabilize.rs` — the pull post-pass: `portabilize_refs(paths, lockfile)` walks every snapshot file, applies `portabilize_value`, rewrites changed files, and re-records the lockfile `content_hash` (sidecar-aware re-hash).
- **Modify:** `src/cli/pull/mod.rs` — add `pub mod portabilize;`.
- **Modify:** `src/cli/sync/execute.rs` (after the per-kind pull dispatch, ~line 3146 in `execute_pull`) — call `portabilize_refs`.
- **Modify (push resolution):** `src/cli/push/{queues,hooks,rules,schemas,inboxes,labels,engines,engine_fields,workspaces,email_templates}.rs` — call `resolve_value(&mut payload, lockfile)` immediately after parsing each disk file into the `payload: Value`.
- **Test:** `src/snapshot/refs.rs` (`#[cfg(test)]`), `tests/portable_refs.rs` (new integration test).

---

## Task 1: `rdc://` scheme module — parse/format + kind classification

**Files:**
- Create: `src/snapshot/refs.rs`
- Modify: `src/snapshot/mod.rs`

- [ ] **Step 1: Write the failing test**

Append to `src/snapshot/refs.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rdc_ref_splits_kind_and_slug() {
        assert_eq!(parse_rdc_ref("rdc://queues/invoices"), Some(("queues", "invoices")));
        // composite slug (engine_fields, email_templates): kind = first segment, slug = remainder
        assert_eq!(parse_rdc_ref("rdc://engine_fields/mtr/code"), Some(("engine_fields", "mtr/code")));
        assert_eq!(parse_rdc_ref("https://x.rossum.app/api/v1/queues/1"), None);
        assert_eq!(parse_rdc_ref("not a ref"), None);
        assert_eq!(parse_rdc_ref("rdc://queues"), None); // no slug
    }

    #[test]
    fn portable_kinds_exclude_externals() {
        assert!(is_portable_kind("queues"));
        assert!(is_portable_kind("workspaces"));
        assert!(is_portable_kind("hooks"));
        assert!(!is_portable_kind("organization"));
        assert!(!is_portable_kind("mdh_indexes"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib snapshot::refs::tests`
Expected: FAIL — `cannot find function parse_rdc_ref` / `is_portable_kind` / module `refs` not declared.

- [ ] **Step 3: Write minimal implementation**

Create `src/snapshot/refs.rs` (above the test module):

```rust
//! The `rdc://<kind>/<slug>` portable-reference scheme.
//!
//! On disk, an internal cross-reference (e.g. `queue.workspace`) is stored as
//! `rdc://<kind>/<slug>` instead of a live API URL, making the snapshot
//! environment-agnostic. `<kind>/<slug>` is exactly the lockfile coordinate
//! `objects[kind][slug]`. Resolution is purely mechanical and needs no
//! per-field schema: walk every string, act on any `rdc://…`.

use crate::state::Lockfile;
use serde_json::Value;

/// `rdc://` URI prefix.
pub const RDC_SCHEME: &str = "rdc://";

/// Kinds whose object URLs are rewritten to `rdc://` on pull. A kind is
/// portable iff rdc snapshots it as a deployable object. `organization`
/// (per-env singleton) and `mdh_indexes` (no `/api/v1/` URL) are excluded;
/// their URLs stay verbatim. Non-snapshotted targets (users, hook_templates)
/// never resolve via the lockfile, so they are left alone regardless.
pub fn is_portable_kind(kind: &str) -> bool {
    !matches!(kind, "organization" | "mdh_indexes")
}

/// Parse `rdc://<kind>/<slug>` into `(kind, slug)`. `kind` is the first path
/// segment; `slug` is the remainder (may itself contain `/` for composite
/// keys like `engine_fields`/`email_templates`). Returns `None` for any
/// string that is not a well-formed `rdc://` ref.
pub fn parse_rdc_ref(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix(RDC_SCHEME)?;
    let (kind, slug) = rest.split_once('/')?;
    if kind.is_empty() || slug.is_empty() {
        return None;
    }
    Some((kind, slug))
}

/// Recursively apply `f` to every string leaf in a JSON tree (objects and
/// array elements, at any depth). Lifted from `deploy/common.rs` so both
/// the portable-ref conversion and the (soon-removed) URL rewriter share it.
pub fn walk_strings_mut(value: &mut Value, f: &mut dyn FnMut(&mut String)) {
    match value {
        Value::String(s) => f(s),
        Value::Array(items) => {
            for item in items {
                walk_strings_mut(item, f);
            }
        }
        Value::Object(map) => {
            for (_k, v) in map.iter_mut() {
                walk_strings_mut(v, f);
            }
        }
        _ => {}
    }
}

/// URL → `rdc://<kind>/<slug>` if the URL belongs to a portable kind tracked
/// in `lockfile`. Returns `None` (leave unchanged) otherwise — externals,
/// `organization`, and unknown URLs all fall here.
pub fn url_to_rdc(url: &str, lockfile: &Lockfile) -> Option<String> {
    let (kind, slug) = lockfile.lookup_url(url)?;
    if !is_portable_kind(kind) {
        return None;
    }
    Some(format!("{RDC_SCHEME}{kind}/{slug}"))
}

/// `rdc://<kind>/<slug>` → the env URL for that object, via the lockfile.
/// Returns `None` if the string is not an `rdc://` ref or the slug is not in
/// the lockfile (a dangling ref — left as-is so the API surfaces a clear error).
pub fn rdc_to_url(s: &str, lockfile: &Lockfile) -> Option<String> {
    let (kind, slug) = parse_rdc_ref(s)?;
    lockfile.url_for_slug(kind, slug).map(|u| u.to_string())
}

/// Pull side: rewrite every portable-kind URL in `value` to `rdc://` form.
pub fn portabilize_value(value: &mut Value, lockfile: &Lockfile) {
    walk_strings_mut(value, &mut |s| {
        if let Some(rdc) = url_to_rdc(s, lockfile) {
            *s = rdc;
        }
    });
}

/// Push side: resolve every `rdc://` ref in `value` to the env URL.
pub fn resolve_value(value: &mut Value, lockfile: &Lockfile) {
    walk_strings_mut(value, &mut |s| {
        if let Some(url) = rdc_to_url(s, lockfile) {
            *s = url;
        }
    });
}
```

Add to `src/snapshot/mod.rs` (with the other `pub mod` lines):

```rust
pub mod refs;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib snapshot::refs::tests`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/refs.rs src/snapshot/mod.rs
git commit -m "feat(refs): rdc://<kind>/<slug> scheme module (parse/format, url<->rdc conversion)"
```

---

## Task 2: Round-trip invariant (URL → `rdc://` → URL) — the keystone test

**Files:**
- Test: `src/snapshot/refs.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/snapshot/refs.rs`:

```rust
    use crate::state::{Lockfile, ObjectEntry};

    fn lf_with(kind: &str, slug: &str, id: u64, url: &str) -> Lockfile {
        let mut lf = Lockfile::default();
        lf.upsert(kind, slug, ObjectEntry {
            id, url: Some(url.into()), modified_at: None, content_hash: None, secrets_hash: None,
        });
        lf
    }

    #[test]
    fn url_round_trips_through_rdc() {
        let url = "https://ferguson-dev.rossum.app/api/v1/workspaces/1054061";
        let lf = lf_with("workspaces", "demo", 1054061, url);
        let rdc = url_to_rdc(url, &lf).unwrap();
        assert_eq!(rdc, "rdc://workspaces/demo");
        assert_eq!(rdc_to_url(&rdc, &lf).as_deref(), Some(url));
    }

    #[test]
    fn organization_and_unknown_urls_are_left_as_urls() {
        let org = "https://ferguson-dev.rossum.app/api/v1/organizations/418975";
        let mut lf = lf_with("organization", "self", 418975, org);
        // a user URL not tracked at all
        let user = "https://ferguson-dev.rossum.app/api/v1/users/499604";
        assert_eq!(url_to_rdc(org, &lf), None);   // organization is not portable
        assert_eq!(url_to_rdc(user, &lf), None);  // not in lockfile
        // nested + array conversion only touches portable refs
        lf.upsert("queues", "invoices", 10, "https://x/api/v1/queues/10".into_entry());
        let mut v = serde_json::json!({
            "queue": "https://x/api/v1/queues/10",
            "organization": org,
            "actions": [{ "payload": { "queue": "https://x/api/v1/queues/10" } }],
        });
        portabilize_value(&mut v, &lf);
        assert_eq!(v["queue"], "rdc://queues/invoices");
        assert_eq!(v["actions"][0]["payload"]["queue"], "rdc://queues/invoices");
        assert_eq!(v["organization"], org); // untouched
    }
```

Add this tiny test helper at the top of the `tests` module (avoids repeating the struct literal):

```rust
    trait IntoEntry { fn into_entry(self) -> crate::state::ObjectEntry; }
    impl IntoEntry for &str {
        fn into_entry(self) -> crate::state::ObjectEntry {
            crate::state::ObjectEntry { id: 10, url: Some(self.into()), modified_at: None, content_hash: None, secrets_hash: None }
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib snapshot::refs::tests::url_round_trips_through_rdc snapshot::refs::tests::organization_and_unknown_urls_are_left_as_urls`
Expected: PASS immediately is NOT acceptable — these exercise Task 1 code, so they should PASS. If any FAIL, fix Task 1's implementation (these are the real behavior guards). Re-run until PASS.

> Note: Task 1's functions already implement this behavior; Task 2 is the assertion that they do. If it passes first try, that's correct — proceed.

- [ ] **Step 3: (no new impl expected)** — only fix Task 1 if a guard fails.

- [ ] **Step 4: Run the full refs test module**

Run: `cargo test --lib snapshot::refs`
Expected: PASS (all).

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/refs.rs
git commit -m "test(refs): url<->rdc round-trip + externals-left-as-urls invariants"
```

---

## Task 3: `is_url` recognizes `rdc://` (keeps ref-arrays order-insensitive)

**Files:**
- Modify: `src/snapshot/noise.rs:164`
- Test: `src/snapshot/noise.rs` (`#[cfg(test)]`)

**Why:** `canonicalize_for_hash` calls `sort_url_arrays`, which sorts an array only when *every* element is a URL (`is_url`). After conversion, ref-arrays (`hook.queues`, `engine.training_queues`) hold `rdc://` strings; `is_url` must accept them so those arrays stay order-insensitive in the hash (the same drift fix already in place for URL arrays).

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/snapshot/noise.rs`:

```rust
    #[test]
    fn canonicalize_is_rdc_ref_array_order_insensitive() {
        let a = br#"{"queues":["rdc://queues/b","rdc://queues/a"]}"#;
        let b = br#"{"queues":["rdc://queues/a","rdc://queues/b"]}"#;
        assert_eq!(canonicalize_for_hash(a), canonicalize_for_hash(b));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib snapshot::noise::tests::canonicalize_is_rdc_ref_array_order_insensitive`
Expected: FAIL — arrays differ because `is_url` rejects `rdc://`, so `sort_url_arrays` leaves order intact and the two canonical forms differ.

- [ ] **Step 3: Write minimal implementation**

In `src/snapshot/noise.rs`, change `is_url`:

```rust
fn is_url(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://") || s.starts_with("rdc://")
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib snapshot::noise`
Expected: PASS — including the pre-existing `canonicalize_preserves_non_url_array_order` (mixed/non-ref arrays still untouched).

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/noise.rs
git commit -m "feat(noise): is_url recognizes rdc:// so ref-arrays stay order-insensitive in the hash"
```

---

## Task 4: Lift `walk_strings_mut` out of deploy; deploy reuses the shared one

**Files:**
- Modify: `src/cli/deploy/common.rs` (delete local `walk_strings_mut` at ~line 275; update `rewrite_urls` to call `crate::snapshot::refs::walk_strings_mut`)

- [ ] **Step 1: Write the failing test** — none new; this is a refactor guarded by the existing deploy tests + the compiler. Skip to Step 2.

- [ ] **Step 2: Make the change**

In `src/cli/deploy/common.rs`:
1. Delete the local `fn walk_strings_mut(...)` definition (~line 275).
2. In `rewrite_urls`, change the call from `walk_strings_mut(value, &mut |s| { ... })` to `crate::snapshot::refs::walk_strings_mut(value, &mut |s| { ... })`.

- [ ] **Step 3: Run to verify it compiles + deploy tests still pass**

Run: `cargo test --lib cli::deploy::common`
Expected: PASS — `rewrite_urls` behavior unchanged (same walker).

- [ ] **Step 4: Confirm no other references to the deleted fn**

Run: `grep -rn 'fn walk_strings_mut' src/`
Expected: exactly one match — `src/snapshot/refs.rs`.

- [ ] **Step 5: Commit**

```bash
git add src/cli/deploy/common.rs
git commit -m "refactor(deploy): use shared snapshot::refs::walk_strings_mut"
```

---

## Task 5: Pull post-pass — `portabilize_refs` (convert files + re-record hashes)

**Files:**
- Create: `src/cli/pull/portabilize.rs`
- Modify: `src/cli/pull/mod.rs` (add `pub mod portabilize;`)
- Test: `src/cli/pull/portabilize.rs` (`#[cfg(test)]`)

**Behavior:** after all per-kind drivers have written snapshots (raw URLs) and recorded lockfile entries, walk every tracked object's file, convert portable-kind URLs to `rdc://`, and if the file changed, rewrite it and update the lockfile `content_hash` to the new on-disk form. The re-hash mirrors how each kind is hashed elsewhere (sidecar-aware), so the next `sync` sees `Clean`.

- [ ] **Step 1: Write the failing test**

Create `src/cli/pull/portabilize.rs` with the implementation stub + tests (stub first so it compiles-then-fails):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Lockfile, ObjectEntry};
    use crate::paths::Paths;

    #[test]
    fn portabilizes_a_flat_object_file_and_rebaselines_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        // a label file referencing nothing portable + a workspace it points at
        let ws_url = "https://x.rossum.app/api/v1/workspaces/5";
        std::fs::create_dir_all(paths.env_root().join("labels")).unwrap();
        let label_path = paths.env_root().join("labels/team-a.json");
        std::fs::write(&label_path, format!(r#"{{"name":"Team A","x":"{ws_url}"}}"#)).unwrap();

        let mut lf = Lockfile::default();
        lf.upsert("workspaces", "main", ObjectEntry { id: 5, url: Some(ws_url.into()), modified_at: None, content_hash: None, secrets_hash: None });
        // label's own pre-conversion hash recorded as base
        let pre = crate::state::content_hash(&std::fs::read(&label_path).unwrap());
        lf.upsert("labels", "team-a", ObjectEntry { id: 9, url: Some("https://x.rossum.app/api/v1/labels/9".into()), modified_at: None, content_hash: Some(pre.clone()), secrets_hash: None });

        portabilize_refs(&paths, &mut lf).unwrap();

        let after = std::fs::read_to_string(&label_path).unwrap();
        assert!(after.contains("rdc://workspaces/main"), "URL should be converted: {after}");
        assert!(!after.contains(ws_url), "raw URL should be gone");
        // lockfile rebaselined to the new on-disk form
        let new_hash = lf.objects["labels"]["team-a"].content_hash.clone().unwrap();
        assert_ne!(new_hash, pre);
        assert_eq!(new_hash, crate::state::content_hash(&std::fs::read(&label_path).unwrap()));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli::pull::portabilize`
Expected: FAIL — `cannot find function portabilize_refs`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/cli/pull/portabilize.rs`:

```rust
//! Pull post-pass: rewrite portable-kind URLs in the just-written snapshot to
//! `rdc://<kind>/<slug>` and re-record the lockfile `content_hash` so the next
//! sync sees `Clean`. Runs after all per-kind drivers (all slugs are known),
//! which is required because forward/intra-kind refs (e.g. `hook.run_after`)
//! cannot be resolved until every object has a slug.

use crate::paths::Paths;
use crate::snapshot::codec::{self, combined_hash};
use crate::snapshot::refs::portabilize_value;
use crate::state::{content_hash, Lockfile};
use anyhow::{Context, Result};
use serde_json::Value;

/// Re-hash an on-disk object the same way its kind is hashed elsewhere
/// (sidecar-aware). `json_path` is the object's JSON file.
fn rehash_on_disk(kind: &str, json_path: &std::path::Path) -> Result<String> {
    let json = std::fs::read(json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let hash = match kind {
        "hooks" => {
            // hook code sidecar: <stem>.py or <stem>.js next to the json
            let code = read_first_sidecar(json_path, &["py", "js"])?;
            crate::state::hook_combined_hash(&json, &code)
        }
        "rules" => {
            let code = read_first_sidecar(json_path, &["py"])?;
            crate::state::rule_combined_hash(&json, &code)
        }
        "schemas" => {
            // formulas live in <dir>/formulas/*.py
            let formulas = read_formula_sidecars(json_path)?;
            crate::state::schema_combined_hash(&json, &formulas)
        }
        _ => content_hash(&json),
    };
    Ok(hash)
}

fn read_first_sidecar(json_path: &std::path::Path, exts: &[&str]) -> Result<Option<String>> {
    for ext in exts {
        let p = json_path.with_extension(ext);
        if p.exists() {
            return Ok(Some(std::fs::read_to_string(&p)
                .with_context(|| format!("reading {}", p.display()))?));
        }
    }
    Ok(None)
}

fn read_formula_sidecars(json_path: &std::path::Path) -> Result<Vec<(String, Vec<u8>)>> {
    let dir = json_path.parent().unwrap().join("formulas");
    let mut out = Vec::new();
    if dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("py") {
                let name = format!("formulas/{}", p.file_name().unwrap().to_string_lossy());
                out.push((name, std::fs::read(&p)?));
            }
        }
    }
    Ok(out)
}

/// Walk every tracked object's JSON file; convert portable-kind URLs to
/// `rdc://`; if changed, rewrite the file and update the lockfile hash.
pub fn portabilize_refs(paths: &Paths, lockfile: &mut Lockfile) -> Result<()> {
    // Snapshot the (kind, slug) list first to avoid borrowing while mutating.
    let targets: Vec<(String, String)> = lockfile
        .objects
        .iter()
        .flat_map(|(kind, m)| m.keys().map(move |slug| (kind.clone(), slug.clone())))
        .collect();

    for (kind, slug) in targets {
        let Some(codec) = codec::codec(&kind) else { continue };
        let json_path = codec.path(paths, &slug);
        if !json_path.exists() {
            continue;
        }
        let bytes = std::fs::read(&json_path)
            .with_context(|| format!("reading {}", json_path.display()))?;
        let mut value: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue, // not JSON (defensive)
        };
        let before = value.clone();
        portabilize_value(&mut value, lockfile);
        if value == before {
            continue; // nothing portable to convert
        }
        let new_bytes = {
            let mut s = serde_json::to_vec_pretty(&value).context("serializing portabilized json")?;
            s.push(b'\n');
            s
        };
        crate::snapshot::writer::write_atomic(&json_path, &new_bytes)
            .with_context(|| format!("writing portabilized {}", json_path.display()))?;
        let new_hash = rehash_on_disk(&kind, &json_path)?;
        if let Some(entry) = lockfile.objects.get_mut(&kind).and_then(|m| m.get_mut(&slug)) {
            entry.content_hash = Some(new_hash);
        }
    }
    Ok(())
}

// Silence unused import in builds where combined_hash isn't directly used here.
#[allow(unused_imports)]
use combined_hash as _combined_hash;
```

> If `codec.path()` for a kind does not point at the object's primary JSON (e.g. mdh), `codec::codec(kind)` returning `None` or a non-existent path makes the loop skip it — safe. Confirm during execution that `path()` returns the `.json` for queues/schemas/inboxes (nested) and flat kinds.

Add to `src/cli/pull/mod.rs`:

```rust
pub mod portabilize;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib cli::pull::portabilize`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/pull/portabilize.rs src/cli/pull/mod.rs
git commit -m "feat(pull): portabilize_refs post-pass — URL->rdc:// on disk + hash rebaseline"
```

---

## Task 6: Wire the post-pass into the pull flow

**Files:**
- Modify: `src/cli/sync/execute.rs` (in `execute_pull`, after the per-kind pull dispatch — grounding places this just after `pull::mdh::process` at ~line 3146, before the function returns)

- [ ] **Step 1: Write the failing test** — covered by the end-to-end Task 9 integration test. Skip to Step 2.

- [ ] **Step 2: Make the change**

In `src/cli/sync/execute.rs`, locate the end of the per-kind pull dispatch in `execute_pull` (after `pull::mdh::process(...)`), and before the function returns, add:

```rust
    // Post-pass: convert portable-kind URLs in the just-written snapshot to
    // rdc://<kind>/<slug> and rebaseline lockfile hashes (env-portable form).
    crate::cli::pull::portabilize::portabilize_refs(ctx.paths, ctx.lockfile)
        .context("portabilizing snapshot references")?;
```

(Use the exact field names from `PullCtx`: `ctx.paths`, `ctx.lockfile`. If `execute_pull` holds `paths`/`lockfile` in locals rather than `ctx`, pass those.)

- [ ] **Step 3: Run to verify it compiles + lib tests pass**

Run: `cargo test --lib cli::sync`
Expected: PASS (compiles; existing sync tests unaffected — they don't assert URL form).

- [ ] **Step 4: Commit**

```bash
git add src/cli/sync/execute.rs
git commit -m "feat(pull): run portabilize_refs after all pull drivers"
```

---

## Task 7: Push resolution — `resolve_value` in every push driver

**Files:**
- Modify: `src/cli/push/queues.rs`, `hooks.rs`, `rules.rs`, `schemas.rs`, `inboxes.rs`, `labels.rs`, `engines.rs`, `engine_fields.rs`, `workspaces.rs`, `email_templates.rs`

**Rule (uniform):** in each push driver, immediately after parsing a disk file into the `payload: serde_json::Value` (and after any `apply_overrides`), before `strip_*`/`from_value`/send, insert:

```rust
crate::snapshot::refs::resolve_value(&mut payload, lockfile);
```

This converts `rdc://<kind>/<slug>` → the **current env's** URL via the env lockfile, for both CREATE (`Value` body) and PATCH (`Value` parsed before `from_value::<Typed>`). Grounded insertion points:

- `queues.rs`: CREATE — after `let mut payload = from_slice(...)` (line ~46), before `strip_for_create` (line 51). PATCH — after `from_slice` into `payload` (line ~98), before `from_value::<Queue>` (line ~104).
- `rules.rs`: CREATE — after `read_rule_value()` (line ~39), before `strip_for_create` (line 44). PATCH — after parsing `payload` (line ~84), before `from_value::<Rule>`.
- `hooks.rs`: regular CREATE — after building `payload` (before `strip_for_create`, line ~190). store-extension PATCH — on `body` before `update_hook_value` (line ~178). PATCH — on `body` before line 362.
- `schemas.rs`, `inboxes.rs`, `labels.rs`, `engines.rs`, `engine_fields.rs`, `workspaces.rs`, `email_templates.rs`: after the disk file is parsed into `payload`/`body` (`from_slice`/`from_reader`) and before strip/send. Find the single `from_slice`/`serde_json::from_*` of the on-disk file in each `push()` and insert the call right after it.

> The variable holding the env lockfile in each push driver is `lockfile` (the `&mut Lockfile` threaded through push). Use it.

- [ ] **Step 1: Write the failing test**

Add a focused unit test to `src/cli/push/queues.rs` `#[cfg(test)]` (proves resolution happens on the payload before send-shaping):

```rust
    #[test]
    fn resolve_value_rewrites_rdc_refs_to_env_urls() {
        use crate::state::{Lockfile, ObjectEntry};
        let mut lf = Lockfile::default();
        lf.upsert("workspaces", "main", ObjectEntry { id: 7, url: Some("https://e/api/v1/workspaces/7".into()), modified_at: None, content_hash: None, secrets_hash: None });
        let mut payload = serde_json::json!({ "name": "Q", "workspace": "rdc://workspaces/main" });
        crate::snapshot::refs::resolve_value(&mut payload, &lf);
        assert_eq!(payload["workspace"], "https://e/api/v1/workspaces/7");
    }
```

- [ ] **Step 2: Run test to verify it fails, then passes**

Run: `cargo test --lib cli::push::queues::tests::resolve_value_rewrites_rdc_refs_to_env_urls`
Expected: PASS (it exercises Task 1's `resolve_value`). This guards that the call you insert does the right thing. If FAIL, fix `resolve_value`.

- [ ] **Step 3: Insert the `resolve_value(...)` call in all 10 push drivers** per the Rule above.

- [ ] **Step 4: Run to verify push tests pass**

Run: `cargo test --lib cli::push`
Expected: PASS. Existing push tests use real URLs on disk; `resolve_value` is a no-op on non-`rdc://` strings, so they are unaffected.

- [ ] **Step 5: Commit**

```bash
git add src/cli/push/
git commit -m "feat(push): resolve rdc:// refs to env URLs before send in all push drivers"
```

---

## Task 8: Full suite + lints green

**Files:** none (verification + fixups)

- [ ] **Step 1: Run the whole test suite**

Run: `cargo test`
Expected: PASS. Likely fixups:
- `tests/cli_deploy.rs` may now observe `rdc://` in pulled fixtures if a test pulls-then-deploys. Deploy's own `rewrite_urls` still operates on the deploy path (not pull-converted within the same test unless the test runs a full pull). If a deploy test fails, it is because its on-disk fixtures are hand-written URLs that are *not* pull-converted (deploy reads them directly) — those tests stay valid. Only fix a test that actually does a `pull` and then asserts URL form.

- [ ] **Step 2: Clippy + fmt**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: clean. Remove the `#[allow(unused_imports)] use combined_hash as _combined_hash;` shim from Task 5 if `combined_hash` ends up unused — or drop the `combined_hash` import entirely (the post-pass uses `content_hash` + the `*_combined_hash` helpers, not `combined_hash`). Make imports exact.

- [ ] **Step 3: Commit any fixups**

```bash
git add -A
git commit -m "test/chore: green suite + clippy after portable-refs Stage 1"
```

---

## Task 9: End-to-end integration test (pull → on-disk `rdc://`, idempotent, sync-Clean)

**Files:**
- Create: `tests/portable_refs.rs`

- [ ] **Step 1: Write the failing test**

Create `tests/portable_refs.rs` modeled on the existing wiremock+temp-dir tests in `tests/cli_sync.rs`/`tests/cli_deploy.rs` (reuse their server-setup helpers/patterns):

```rust
// Mirror the harness used by tests/cli_sync.rs (wiremock RossumApi mock + temp project).
// 1. Mock a queue whose `workspace`/`schema` are URLs into the mock host.
// 2. Run `rdc sync <env>` (pull side).
// 3. Assert the on-disk queue.json now contains "rdc://workspaces/<slug>" and
//    "rdc://schemas/<slug>", and NO "http(s)://.../api/v1/workspaces/" URL.
// 4. Run pull again; assert the file is byte-identical (idempotent) and the
//    sync classification reports Clean (no drift) — guards the rebaseline.
```

Implement the three assertions concretely using the project's existing test scaffolding (search `tests/cli_sync.rs` for the mock-server + `run_sync` helper and copy its setup). The key assertions:

```rust
let q = std::fs::read_to_string(queue_json_path).unwrap();
assert!(q.contains("rdc://workspaces/"), "workspace ref portabilized: {q}");
assert!(q.contains("rdc://schemas/"), "schema ref portabilized");
assert!(!q.contains("/api/v1/workspaces/"), "no raw workspace URL remains");
// idempotency + Clean after second pull
let q2 = std::fs::read_to_string(queue_json_path).unwrap();
assert_eq!(q, q2, "second pull is byte-identical (rebaselined)");
```

- [ ] **Step 2: Run test to verify it fails (before Tasks 5–7 wired) / passes (after)**

Run: `cargo test --test portable_refs`
Expected: PASS now that pull conversion (Tasks 5–6) and the rebaseline are in place. If the second-pull assertion fails (drift), the re-hash in Task 5 is wrong for that kind — fix `rehash_on_disk` to match that kind's hashing.

- [ ] **Step 3: Commit**

```bash
git add tests/portable_refs.rs
git commit -m "test(portable-refs): e2e pull writes rdc://, idempotent, sync-Clean"
```

---

## Task 10: Non-destructive live verification

**Files:** none (manual verification, recorded in commit message)

- [ ] **Step 1: Build the local binary**

Run: `cargo build --release`

- [ ] **Step 2: Verify on a temp copy of a real snapshot (never the live original)**

```bash
# Copy a real project to a scratch dir so the original is untouched.
cp -R /Users/martin.zlamal@rossum.ai/Work/gitlab.rossum.cloud/ferguson/ferguson-us2-rdc /tmp/rdc-portable-verify
cd /tmp/rdc-portable-verify
# Pull dev-mtr with the freshly built binary (NOT the Homebrew rdc — use the absolute path).
/Users/martin.zlamal@rossum.ai/Work/github.com/mrtnzlml/rdc/target/release/rdc sync dev-mtr   # provide a valid token when prompted
```

- [ ] **Step 3: Confirm the three properties**

```bash
# (a) snapshot now stores rdc:// refs, no raw /api/v1/ object URLs in queue/hook bodies
grep -rl 'rdc://' envs/dev-mtr/workspaces | head
grep -rn '"workspace": "https' envs/dev-mtr/workspaces || echo "no raw workspace URLs ✓"
# (b) a second pull is a no-op (Clean) — run sync again, expect "up to date" / no drift
/Users/.../target/release/rdc sync dev-mtr
# (c) (read-only) deploy --dry-run still produces a plan without panicking
/Users/.../target/release/rdc deploy dev-mtr test-mtr --dry-run | tail
```

Expected: (a) refs are `rdc://`; (b) second pull reports no changes (rebaseline holds); (c) deploy dry-run still runs.

- [ ] **Step 4: Record the verification in a commit (docs/notes only — no code)**

```bash
cd /Users/martin.zlamal@rossum.ai/Work/github.com/mrtnzlml/rdc
git commit --allow-empty -m "verify(portable-refs): live dev-mtr pull writes rdc://, idempotent, deploy dry-run OK"
```

---

## Self-Review

- **Spec coverage:** This plan implements spec §3.1 (`rdc://` convention — Task 1), §3.2 internal-ref taxonomy (portable-kind gate + nested/array conversion — Tasks 1–2), §3.3 pull post-pass (Tasks 5–6), §3.4 push resolution (Task 7), §3.5 ref-array order-insensitivity (Task 3), and the §7 one-time rebaseline (Task 5 re-hash + Task 9 idempotency guard). **Out of scope by design (later stages):** §3.6 lockfile v3, §3.7 mapping v2, §4 commands, §5 deploy removal, §8 MDH cross-env, §6 rename guards (already satisfied by existing id-pinned slugs; no code here).
- **Placeholder scan:** No "TBD/TODO". Task 7's per-driver insertions give a uniform rule + grounded line numbers for the drivers read during grounding (queues/hooks/rules) and a precise "after the disk `from_slice`" rule for the rest — concrete, not a placeholder.
- **Type consistency:** `walk_strings_mut`, `url_to_rdc`, `rdc_to_url`, `portabilize_value`, `resolve_value`, `parse_rdc_ref`, `is_portable_kind` are defined once in Task 1 and used verbatim thereafter. `portabilize_refs(&Paths, &mut Lockfile)` and `rehash_on_disk(&str, &Path)` match their call sites. `combined_hash(json, sidecars)` / `content_hash(&[u8])` / `hook_combined_hash` / `rule_combined_hash` / `schema_combined_hash` match the grounded signatures.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-05-portable-refs-stage1.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
