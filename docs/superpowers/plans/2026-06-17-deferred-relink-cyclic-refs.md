# Deferred Relink for Cyclic References — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `rdc sync` complete when the snapshot contains engine↔queue reference cycles, by creating ref-clean skeletons first and PATCHing the cross-references in a relink phase once every object exists.

**Architecture:** Per-kind push drivers defer any top-level field still holding an unresolved `rdc://` ref (recording it), push the ref-clean skeleton, then a relink phase (run in `execute.rs` after `push_classified`, before `portabilize_refs`) re-resolves and PATCHes the deferred fields. Apply-all-then-fail-loud on anything still unresolvable. Idempotency is preserved by the relink doing the same post-write bookkeeping as a normal push plus the existing `portabilize_refs` post-pass.

**Tech Stack:** Rust, `serde_json`, `anyhow`, `tokio` (tests), existing rdc modules (`snapshot::refs`, `snapshot::codec`, `state::Lockfile`, `cli::pull::common::record_object`, `state::base_cache::write_disk_and_cache`).

**Spec:** `docs/superpowers/specs/2026-06-17-deferred-relink-cyclic-refs-design.md`

**Prerequisite (NOT in this plan):** test's stale lockfile ids (§7 of the spec) must be re-pinned before the `training_queues` relink can resolve on that project. This plan assumes correct lockfile ids.

**Already shipped (do not redo):** `refs::residual_rdc_refs` + `refs::walk_strings`, and `api::ensure_no_residual_refs` wired into `post_json`/`patch_json` (the hard backstop). 736 lib tests green at plan time.

---

## File structure

- `src/snapshot/refs.rs` — add `resolve_value_deferring` (pure). Tests inline.
- `src/cli/push/relink.rs` — **new** module: `DeferredRelink` struct, the pure `resolve_relink_body` helper, and the async `run_relink` phase. Tests inline (pure parts).
- `src/cli/push/mod.rs` — declare `pub mod relink;`; thread `&mut Vec<DeferredRelink>` through `push_classified`.
- `src/cli/push/queues.rs`, `src/cli/push/engines.rs` — defer via the new helper; engine CREATE non-fatal on 403/405.
- `src/api/mod.rs` — add `pub async fn patch_value(path, &Value) -> Result<Value>`.
- `src/cli/sync/execute.rs` — own the accumulator, call `run_relink` after `push_classified`, before `portabilize_refs`; aggregate fail-loud.

---

## Task 1: Deferral helper `resolve_value_deferring`

**Files:**
- Modify: `src/snapshot/refs.rs`
- Test: `src/snapshot/refs.rs` (inline `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/snapshot/refs.rs`:

```rust
#[test]
fn resolve_value_deferring_defers_unresolved_top_level_fields_only() {
    // `invoices` is pinned; `1-inbox-sorting` engine is NOT.
    let api_base = "https://example.rossum.app/api/v1";
    let lf = lf_with(api_base, "queues", "invoices", 10);
    let mut body = serde_json::json!({
        "name": "Q",
        "workspace": "rdc://queues/invoices",            // resolvable -> stays, rewritten
        "engine": "rdc://engines/1-inbox-sorting",    // dangling -> deferred + removed
    });
    let deferred = resolve_value_deferring(&mut body, &lf);
    // resolvable ref was rewritten in place
    assert_eq!(body["workspace"], format!("{api_base}/queues/10"));
    // dangling field removed from the body
    assert!(body.get("engine").is_none(), "deferred field must be removed: {body}");
    // and returned for later relink, with its original value
    assert_eq!(deferred, vec![("engine".to_string(),
        serde_json::json!("rdc://engines/1-inbox-sorting"))]);
}

#[test]
fn resolve_value_deferring_defers_array_field_with_any_unresolved_member() {
    let api_base = "https://example.rossum.app/api/v1";
    let lf = lf_with(api_base, "queues", "invoices", 10);
    let mut body = serde_json::json!({
        "training_queues": ["rdc://queues/invoices", "rdc://queues/missing"],
    });
    let deferred = resolve_value_deferring(&mut body, &lf);
    assert!(body.get("training_queues").is_none());
    // whole field deferred; original (pre-resolution) value preserved for relink
    assert_eq!(deferred, vec![("training_queues".to_string(),
        serde_json::json!(["rdc://queues/invoices", "rdc://queues/missing"]))]);
}

#[test]
fn resolve_value_deferring_defers_nothing_when_fully_resolvable() {
    let api_base = "https://example.rossum.app/api/v1";
    let lf = lf_with(api_base, "queues", "invoices", 10);
    let mut body = serde_json::json!({ "workspace": "rdc://queues/invoices" });
    let deferred = resolve_value_deferring(&mut body, &lf);
    assert!(deferred.is_empty());
    assert_eq!(body["workspace"], format!("{api_base}/queues/10"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib snapshot::refs::tests::resolve_value_deferring -- --nocapture`
Expected: FAIL to compile — `cannot find function resolve_value_deferring`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/snapshot/refs.rs` (after `resolve_value`):

```rust
/// Push side, two-phase. Resolve every `rdc://` ref that CAN be resolved
/// (rewriting it to an env URL in place), then DEFER every **top-level object
/// field** whose value still contains a residual `rdc://` ref — remove it from
/// `value` and return `(field_name, original_value)` so the caller can PATCH it
/// later, once the referenced object exists. The returned value is the ORIGINAL
/// (pre-resolution) field so re-resolving it later is straightforward.
/// Non-object inputs defer nothing.
pub fn resolve_value_deferring(value: &mut Value, lockfile: &Lockfile) -> Vec<(String, Value)> {
    let Some(obj) = value.as_object() else {
        resolve_value(value, lockfile);
        return Vec::new();
    };
    // Identify top-level fields that have any residual rdc:// ref BEFORE resolving.
    let to_defer: Vec<String> = obj
        .iter()
        .filter(|(_, v)| !residual_rdc_refs(v).is_empty())
        .map(|(k, _)| k.clone())
        .collect();
    let obj = value.as_object_mut().expect("checked above");
    let mut deferred = Vec::with_capacity(to_defer.len());
    for k in &to_defer {
        if let Some(orig) = obj.remove(k) {
            deferred.push((k.clone(), orig));
        }
    }
    // Resolve whatever resolvable refs remain in the trimmed body, in place.
    resolve_value(value, lockfile);
    deferred
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib snapshot::refs::tests::resolve_value_deferring -- --nocapture`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/refs.rs
git commit -m "feat(refs): add resolve_value_deferring for two-phase push"
```

---

## Task 2: `DeferredRelink` + pure `resolve_relink_body`

**Files:**
- Create: `src/cli/push/relink.rs`
- Modify: `src/cli/push/mod.rs` (add `pub mod relink;`)
- Test: `src/cli/push/relink.rs` (inline)

- [ ] **Step 1: Create the module skeleton + declare it**

Create `src/cli/push/relink.rs`:

```rust
//! Two-phase push relink: PATCH cross-references that were deferred during the
//! skeleton create/patch because their target object did not yet exist.

use crate::snapshot::refs::{residual_rdc_refs, resolve_value};
use crate::state::Lockfile;
use serde_json::{Map, Value};

/// One object that had ≥1 cross-reference field deferred during push. The
/// object itself (`kind`/`slug`) already exists + is lockfile-pinned; `fields`
/// are the `(name, original_rdc_value)` pairs to re-resolve and PATCH.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredRelink {
    pub kind: String,
    pub slug: String,
    pub fields: Vec<(String, Value)>,
}

/// Re-resolve every deferred field against the now-complete lockfile.
/// Returns `Ok(patch_body)` when all fields fully resolve, or `Err(unresolved)`
/// listing every `rdc://` ref that still has no target (the referenced object
/// was never created — e.g. an engine whose create was 403-skipped).
pub fn resolve_relink_body(
    fields: &[(String, Value)],
    lockfile: &Lockfile,
) -> Result<Map<String, Value>, Vec<String>> {
    let mut body = Map::new();
    let mut unresolved = Vec::new();
    for (name, orig) in fields {
        let mut v = orig.clone();
        resolve_value(&mut v, lockfile);
        let residual = residual_rdc_refs(&v);
        if residual.is_empty() {
            body.insert(name.clone(), v);
        } else {
            unresolved.extend(residual);
        }
    }
    if unresolved.is_empty() {
        Ok(body)
    } else {
        unresolved.sort();
        unresolved.dedup();
        Err(unresolved)
    }
}
```

Add to `src/cli/push/mod.rs` near the other `mod` lines (after `pub mod deletes;`):

```rust
pub mod relink;
```

- [ ] **Step 2: Write the failing tests**

Append to `src/cli/push/relink.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Lockfile, ObjectEntry};

    fn lf(api_base: &str, kind: &str, slug: &str, id: u64) -> Lockfile {
        let mut lf = Lockfile { api_base: api_base.to_string(), ..Lockfile::default() };
        lf.upsert(kind, slug, ObjectEntry { id, modified_at: None, content_hash: None, secrets_hash: None });
        lf
    }

    #[test]
    fn resolve_relink_body_resolves_when_target_now_exists() {
        let api_base = "https://x.rossum.app/api/v1";
        let lockfile = lf(api_base, "engines", "1-inbox-sorting", 392);
        let fields = vec![("engine".to_string(),
            serde_json::json!("rdc://engines/1-inbox-sorting"))];
        let body = resolve_relink_body(&fields, &lockfile).expect("should resolve");
        assert_eq!(body["engine"], format!("{api_base}/engines/392"));
    }

    #[test]
    fn resolve_relink_body_errors_listing_unresolved() {
        let lockfile = Lockfile { api_base: "https://x.rossum.app/api/v1".into(), ..Lockfile::default() };
        let fields = vec![("engine".to_string(),
            serde_json::json!("rdc://engines/never-created"))];
        let err = resolve_relink_body(&fields, &lockfile).unwrap_err();
        assert_eq!(err, vec!["rdc://engines/never-created".to_string()]);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail, then pass**

Run: `cargo test --lib cli::push::relink:: -- --nocapture`
Expected: first FAIL to compile if module not wired; once Step 1's code is in, the two tests PASS. (Implementation already written in Step 1 — verify green.)

- [ ] **Step 4: Commit**

```bash
git add src/cli/push/relink.rs src/cli/push/mod.rs
git commit -m "feat(push): add DeferredRelink + pure resolve_relink_body"
```

---

## Task 3: Generic `patch_value` on `RossumClient`

**Files:**
- Modify: `src/api/mod.rs`
- Test: `src/api/mod.rs` (inline `tests` module that already exists)

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` in `src/api/mod.rs`:

```rust
#[tokio::test]
async fn patch_value_refuses_unresolved_ref_before_network() {
    let client = RossumClient::new("https://example.invalid/api/v1".to_string(), "t".to_string()).unwrap();
    let body = json!({ "engine": "rdc://engines/missing" });
    let err = client.patch_value("/queues/1", &body, None).await.unwrap_err();
    assert!(format!("{err:#}").contains("rdc://engines/missing"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib api::tests::patch_value_refuses -- --nocapture`
Expected: FAIL to compile — `no method named patch_value`.

- [ ] **Step 3: Write minimal implementation**

Add to `impl RossumClient` in `src/api/mod.rs` (near `update_engine`):

```rust
/// Generic partial PATCH: send `body` to `path` and return the raw updated
/// JSON. Used by the relink phase, which patches just the previously-deferred
/// cross-reference fields of an arbitrary kind. Goes through `patch_json`, so
/// the residual-`rdc://` guard applies.
pub async fn patch_value(&self, path: &str, body: &serde_json::Value, progress: ProgressHandle) -> Result<serde_json::Value> {
    self.patch_json(path, body, progress).await
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib api::tests::patch_value_refuses -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/api/mod.rs
git commit -m "feat(api): add generic patch_value for the relink phase"
```

---

## Task 4: Thread the relink accumulator through `push_classified`

**Files:**
- Modify: `src/cli/push/mod.rs:34` (signature of `push_classified`)

- [ ] **Step 1: Add the parameter**

Change the `push_classified` signature to accept the accumulator (add as the last parameter before `progress`, keep call sites compiling — they are updated in Task 7):

```rust
pub(crate) async fn push_classified(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    env: &str,
    interactive: bool,
    changes: &scan::ChangeList,
    catalog_hooks: &[crate::model::Hook],
    relink: &mut Vec<relink::DeferredRelink>,
    progress: &Arc<Log>,
) -> Result<()> {
```

- [ ] **Step 2: Pass `relink` into the queue + engine drivers**

In the `queues::push(...)` call (currently `mod.rs:53`) and `engines::push(...)` call (currently `mod.rs:81`), add `relink` as a new argument (the driver signatures change in Tasks 5–6). Leave the other drivers unchanged.

- [ ] **Step 3: Verify it compiles after Tasks 5–7**

This task is structural; full compilation is achieved once Tasks 5–7 land. After Task 7:

Run: `cargo build`
Expected: `Finished`.

- [ ] **Step 4: Commit (with Tasks 5–7)** — commit together once the pipeline compiles (see Task 7 Step 5).

---

## Task 5: Defer in `engines::push` + make CREATE non-fatal on 403/405

**Files:**
- Modify: `src/cli/push/engines.rs`

- [ ] **Step 1: Add `relink` parameter + swap resolve calls**

In `engines::push`, add `relink: &mut Vec<crate::cli::push::relink::DeferredRelink>` to the signature (before `progress`). Replace each `crate::snapshot::refs::resolve_value(&mut payload, lockfile);` (the create path ~line 40, the patch path ~line 88, and the drift-override ~line 129) with:

```rust
let deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lockfile);
if !deferred.is_empty() {
    relink.push(crate::cli::push::relink::DeferredRelink {
        kind: "engines".to_string(),
        slug: slug.clone(),
        fields: deferred,
    });
}
```

(Under the current push order engines defer nothing — queues already exist — but wire it for correctness/generality.)

- [ ] **Step 2: Make engine CREATE non-fatal on 403/405**

In the create path, replace `let created = create_result?;` with status-aware handling mirroring the existing PATCH-405 branch (~line 177):

```rust
let created = match create_result {
    Ok(c) => c,
    Err(e) if crate::api::anyhow_has_status(&e, 405) || crate::api::anyhow_has_status(&e, 403) => {
        progress.event(Action::Skip, &format!("engine/{slug} (create {} — engines not writable on this plan)",
            if crate::api::anyhow_has_status(&e, 403) { "403" } else { "405" }));
        skipped += 1;
        continue;
    }
    Err(e) => return Err(e),
};
```

- [ ] **Step 3: Verify existing engine tests + build**

Run: `cargo test --lib snapshot:: && cargo build`
Expected: green (no behavior change for the happy path; the create path now degrades instead of aborting).

- [ ] **Step 4: Commit (with Task 6/7)** — see Task 7 Step 5.

---

## Task 6: Defer in `queues::push`

**Files:**
- Modify: `src/cli/push/queues.rs`

- [ ] **Step 1: Add `relink` parameter + swap resolve calls**

Add `relink: &mut Vec<crate::cli::push::relink::DeferredRelink>` to `queues::push` (before `progress`). Replace each `crate::snapshot::refs::resolve_value(&mut payload, lockfile);` / `resolve_value(&mut ov, lockfile)` (the create path ~line 43, the patch path ~line 91, the drift-override ~line 132) with the deferring variant, recording any deferred fields:

```rust
let deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lockfile);
if !deferred.is_empty() {
    relink.push(crate::cli::push::relink::DeferredRelink {
        kind: "queues".to_string(),
        slug: q_slug.clone(),
        fields: deferred,
    });
}
```

(For the drift-override block, the variable is `ov` and the slug is still `q_slug`.)

- [ ] **Step 2: Verify build**

Run: `cargo build`
Expected: `Finished` (once Task 7 updates the call sites).

- [ ] **Step 3: Commit (with Task 7)** — see Task 7 Step 5.

---

## Task 7: Run the relink phase in `execute.rs` + fail-loud

**Files:**
- Create the async phase: `src/cli/push/relink.rs` (add `run_relink`)
- Modify: `src/cli/sync/execute.rs` (~line 3175 call site + after it)

- [ ] **Step 1: Add the async `run_relink` phase**

Append to `src/cli/push/relink.rs`:

```rust
use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::snapshot::codec::{codec, combined_hash};
use anyhow::Result;
use std::sync::Arc;

/// PATCH every deferred cross-reference now that all objects exist. Applies all
/// resolvable relinks; collects failures (unresolved refs OR API rejections)
/// and returns them so the caller can fail loud after the whole pass.
pub async fn run_relink(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut crate::state::Lockfile,
    items: &[DeferredRelink],
    progress: &Arc<Log>,
) -> Result<Vec<String>> {
    let mut failures = Vec::new();
    for it in items {
        let Some(entry) = lockfile.objects.get(&it.kind).and_then(|m| m.get(&it.slug)) else {
            failures.push(format!("{}/{}: object missing in lockfile (its create was skipped/failed); cannot relink {:?}",
                it.kind, it.slug, it.fields.iter().map(|(k, _)| k).collect::<Vec<_>>()));
            continue;
        };
        let id = entry.id;
        let body = match resolve_relink_body(&it.fields, lockfile) {
            Ok(b) => b,
            Err(unresolved) => {
                failures.push(format!("{}/{}: unresolved reference(s) {:?} — target object not present in this environment",
                    it.kind, it.slug, unresolved));
                continue;
            }
        };
        let path = format!("/{}/{}", it.kind, id); // endpoint == kind for queues/engines
        match client.patch_value(&path, &serde_json::Value::Object(body), Some(progress.clone())).await {
            Ok(updated) => {
                // Post-write bookkeeping mirrors a normal push: rewrite the disk
                // file + re-record the lockfile hash from the relinked object so
                // the subsequent portabilize_refs post-pass (URL -> rdc://) lands
                // on Clean.
                if let Some(c) = codec(&it.kind) {
                    let art = c.disk_bytes(&updated)?;
                    let hash = combined_hash(&art.json, &art.sidecars, lockfile);
                    let file = c.path(paths, &it.slug);
                    crate::state::base_cache::write_disk_and_cache(paths, &file, &art.json)?;
                    let modified_at = updated.get("modified_at").and_then(|v| v.as_str()).map(|s| s.to_string());
                    crate::cli::pull::common::record_object(lockfile, &it.kind, &it.slug, id, modified_at, Some(hash));
                }
                progress.event(Action::Patch, &format!("relink {}/{} {:?}",
                    it.kind, it.slug, it.fields.iter().map(|(k, _)| k).collect::<Vec<_>>()));
            }
            Err(e) => failures.push(format!("{}/{}: PATCH {} rejected: {e:#}", it.kind, it.slug, path)),
        }
    }
    Ok(failures)
}
```

- [ ] **Step 2: Wire it into `execute.rs`**

At the `push_classified(...)` call (~line 3175), declare the accumulator before the call and pass it in, then run the phase and fail loud after:

```rust
let mut relink_items: Vec<crate::cli::push::relink::DeferredRelink> = Vec::new();
crate::cli::push::push_classified(
    ctx.paths, ctx.client, ctx.lockfile, &env, interactive,
    &change_list, &catalog.hooks, &mut relink_items, progress,
).await?;

if !relink_items.is_empty() {
    let failures = crate::cli::push::relink::run_relink(
        ctx.paths, ctx.client, ctx.lockfile, &relink_items, progress,
    ).await?;
    if !failures.is_empty() {
        anyhow::bail!(
            "deferred relink could not complete {} reference(s):\n  - {}",
            failures.len(), failures.join("\n  - ")
        );
    }
}
```

(The `portabilize_refs` post-pass at ~line 3411 then runs as today and finalizes the on-disk `rdc://` form + hashes.)

- [ ] **Step 3: Build + full suite**

Run: `cargo build && cargo test --lib`
Expected: `Finished`; `test result: ok.` (≥ 736 + new tests, 0 failed).

- [ ] **Step 4: Clippy**

Run: `cargo clippy --lib --tests`
Expected: 0 warnings.

- [ ] **Step 5: Commit the pipeline (Tasks 4–7)**

```bash
git add src/cli/push/mod.rs src/cli/push/engines.rs src/cli/push/queues.rs src/cli/push/relink.rs src/cli/sync/execute.rs
git commit -m "feat(sync): two-phase deferred relink for engine<->queue cycles"
```

---

## Task 8: Live acceptance + idempotency (authorized env)

rdc has no mock-HTTP harness, so the relink orchestration is accepted against a live engine-capable env (test) with the user's authorization. The relink PATCH primitives are already live-verified (spec §2).

- [ ] **Step 1: Resolve the stale-lockfile-id prerequisite** for the target project (spec §7) — e.g. `rdc doctor --rebuild-lock test` — so `rdc://queues/…` resolve to live ids. Confirm with `rdc sync test --dry-run`.

- [ ] **Step 2: Run the sync**

Run: `rdc sync test`
Expected: engines POST, engine_fields POST, queues PATCH (engine omitted), then `relink queues/1-inbox-sorting [engine]` lines; **no** `Invalid hyperlink` 400. If any relink fails, it is reported in one aggregated fail-loud error (apply-all-then-fail-loud).

- [ ] **Step 3: Idempotency**

Run: `rdc sync test` (again)
Expected: no pushes/relinks — every object `Clean` (the relink + `portabilize_refs` post-pass recorded matching hashes).

- [ ] **Step 4: Clean up any throwaway verification objects** and record results in the `project-engine-queue-cycle` memory.

---

## Self-review notes
- **Spec coverage:** detection (shipped), deferral (Task 1,5,6), relink phase + placement (Task 2,7), error model/fail-loud (Task 7), engine-create non-fatal (Task 5), idempotency (Task 7 bookkeeping + Task 8), out-of-scope stale ids (Task 8 Step 1). Covered.
- **Type consistency:** `DeferredRelink { kind, slug, fields }`, `resolve_value_deferring -> Vec<(String, Value)>`, `resolve_relink_body -> Result<Map, Vec<String>>`, `patch_value -> Result<Value>`, `run_relink -> Result<Vec<String>>` used consistently across tasks.
- **Known soft spot:** the relink's post-write bookkeeping (Task 7 Step 1) must produce bytes/hashes that match a fresh pull so Task 8 Step 3 (idempotency) passes; if it drifts, iterate there — that test is the gate.
