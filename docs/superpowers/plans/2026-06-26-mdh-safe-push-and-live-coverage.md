# MDH safe-push fix + adversarial live coverage — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rdc sync`'s within-env MDH index push base-aware (admin-added remote-only indexes never silently dropped; genuine user-removals gated behind `--allow-deletes`) and cover the full MDH pull+push surface with an idempotent, isolated live integration scenario.

**Architecture:** A new pure 3-way (base/local/remote) index diff replaces the 2-way `mirror=true` diff inside `push_dataset`; the base leg comes from the existing base-cache sidecar (written by MDH pull). `push_dataset` gains `paths` + `allow_deletes` + `interactive`, gates user-removal drops exactly like rdc's global delete gate, and writes the base cache on a fully-applied push (fixing a latent hash-invariant violation). Live coverage adds a test-only raw Data-Storage helper (3 endpoints) plus a 6-phase scenario on a per-run throwaway collection, with teardown + janitor sweep.

**Tech Stack:** Rust, `serde_json`, `tokio`, `reqwest` (already deps), the existing `tests/live` harness (`#[ignore]`, `RDC_LIVE_*`-gated).

## Global Constraints

- **No customer identifiers anywhere** (source, tests, fixtures, goldens, commit messages): real collection names, org id, host, token must never enter the repo. All live config comes from `RDC_LIVE_API_BASE` / `RDC_LIVE_ORG_ID` / `RDC_LIVE_TOKEN`. The sandbox token is a secret — never commit it.
- **Default `cargo test` stays green.** Live scenarios stay `#[ignore]` and skip-with-message when `RDC_LIVE_*` is unset. Pure-logic additions (`diff_indexes_3way`, the gate classifier, the collection-name helper) have hermetic unit tests that run by default.
- **No on-disk snapshot or lockfile-format change.** `indexes.json` shape, the `mdh_indexes` lockfile kind, and codec output are unchanged.
- **Only production visibility change:** `IndexSet` gains `Default`. No other `pub`/`pub(crate)` widening. The test crate reuses the already-`pub` `DataStorageClient` and `EnvConfig::data_storage_base()`.
- **Idempotent + isolated live coverage:** per-run throwaway collection only; RAII teardown + janitor; never touch a real collection; the whole scenario is re-runnable and a final re-sync performs 0 MDH write ops.
- **Never `git push`** — commit locally only.
- **Commit message trailer:** end every commit with
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
- **`rdc deploy` is unaffected** (MDH not deployable); `diff_indexes(.., mirror)` and `diff_for_dataset` are retained with their tests.

---

### Task 1: Base-aware 3-way index diff (`diff_indexes_3way`)

**Files:**
- Modify: `src/model/index_set.rs:7` (add `Default` to derive)
- Modify: `src/cli/push/mdh.rs` (lift `index_by_name` to module scope; add `ThreeWayDiff` + `diff_indexes_3way` + `diff_one_kind`; add unit tests)

**Interfaces:**
- Consumes: existing `DiffPlan` (`mdh.rs:287`), `defs_equivalent` (`mdh.rs:408`), `serde_json::Value`, `std::collections::BTreeMap`.
- Produces:
  - `IndexSet: Default` (empty `regular`/`search`).
  - `pub(crate) struct ThreeWayDiff { pub plan: DiffPlan, pub pending_regular_deletes: Vec<String>, pub pending_search_deletes: Vec<String> }`
  - `pub(crate) fn diff_indexes_3way(base_regular: &[Value], base_search: &[Value], local_regular: &[Value], local_search: &[Value], remote_regular: &[Value], remote_search: &[Value]) -> ThreeWayDiff`
  - `fn index_by_name(items: &[Value], filter_id_index: bool) -> BTreeMap<String, &Value>` (lifted to module scope; signature unchanged from the nested version).

This task is pure logic with no I/O — fully covered by default `cargo test`.

- [ ] **Step 1: Add `Default` to `IndexSet`**

In `src/model/index_set.rs:7`, change:

```rust
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct IndexSet {
```

to:

```rust
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct IndexSet {
```

- [ ] **Step 2: Lift `index_by_name` to module scope**

In `src/cli/push/mdh.rs`, the function `index_by_name` is currently a nested
`fn` inside `diff_indexes` (around `mdh.rs:328-339`). Remove the nested
definition from inside `diff_indexes` and add it at module scope (e.g. just
above `diff_indexes`), so both `diff_indexes` and the new `diff_one_kind` can
call it. The body is unchanged:

```rust
/// Build a name→def map, optionally filtering the implicit `_id_` regular
/// index (server-managed, can't be dropped). Shared by the 2-way mirror diff
/// and the 3-way base-aware diff.
fn index_by_name(items: &[Value], filter_id_index: bool) -> BTreeMap<String, &Value> {
    let mut out: BTreeMap<String, &Value> = BTreeMap::new();
    for ix in items {
        if let Some(name) = ix.get("name").and_then(|v| v.as_str()) {
            if filter_id_index && name == "_id_" {
                continue;
            }
            out.insert(name.to_string(), ix);
        }
    }
    out
}
```

`diff_indexes` keeps using `index_by_name(...)` exactly as before (it now
resolves to the module-scope fn). Do not change `diff_indexes`'s behavior.

- [ ] **Step 3: Write the failing unit tests for `diff_indexes_3way`**

Add to the `#[cfg(test)] mod tests` block at the bottom of `src/cli/push/mdh.rs`.
These reuse the existing `ix(name, key)` test helper (`mdh.rs:451`).

```rust
// --- 3-way (base-aware) diff: the within-env safe-push semantics ---

#[test]
fn three_way_admin_added_remote_only_survives() {
    // Admin added ix_admin remotely; it's not in base and not in local.
    // It must NOT be dropped and must NOT become a pending delete.
    let base = vec![ix("ix_keep", json!({"k": 1}))];
    let local = vec![ix("ix_keep", json!({"k": 1}))];
    let remote = vec![ix("ix_keep", json!({"k": 1})), ix("ix_admin", json!({"a": 1}))];
    let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
    assert!(d.plan.drop_regular.is_empty(), "{d:?}");
    assert!(d.pending_regular_deletes.is_empty(), "{d:?}");
    assert!(d.plan.create_regular.is_empty(), "{d:?}");
}

#[test]
fn three_way_user_removed_index_is_pending_delete() {
    // ix_gone was in base, removed from local, still on remote -> pending.
    let base = vec![ix("ix_keep", json!({"k": 1})), ix("ix_gone", json!({"g": 1}))];
    let local = vec![ix("ix_keep", json!({"k": 1}))];
    let remote = vec![ix("ix_keep", json!({"k": 1})), ix("ix_gone", json!({"g": 1}))];
    let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
    assert_eq!(d.pending_regular_deletes, vec!["ix_gone".to_string()]);
    assert!(d.plan.drop_regular.is_empty(), "pending != plan-drop: {d:?}");
    assert!(d.plan.create_regular.is_empty());
}

#[test]
fn three_way_local_only_index_is_created() {
    let base = vec![];
    let local = vec![ix("ix_new", json!({"n": 1}))];
    let remote = vec![];
    let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
    assert_eq!(d.plan.create_regular.len(), 1);
    assert!(d.plan.drop_regular.is_empty());
    assert!(d.pending_regular_deletes.is_empty());
}

#[test]
fn three_way_changed_def_is_drop_and_create_not_pending() {
    // Same name, diverging def -> always drop+recreate, never gated.
    let base = vec![ix("ix_x", json!({"x": 1}))];
    let local = vec![ix("ix_x", json!({"x": -1}))];
    let remote = vec![ix("ix_x", json!({"x": 1}))];
    let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
    assert_eq!(d.plan.drop_regular, vec!["ix_x".to_string()]);
    assert_eq!(d.plan.create_regular.len(), 1);
    assert!(d.pending_regular_deletes.is_empty());
}

#[test]
fn three_way_no_base_never_pends_deletes() {
    // No base (empty) + a remote-only index not in local -> can't prove a
    // user removal, so NO pending delete (strictly-safe fallback). A local
    // create still happens.
    let base = vec![];
    let local = vec![ix("ix_new", json!({"n": 1}))];
    let remote = vec![ix("ix_admin", json!({"a": 1}))];
    let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
    assert_eq!(d.plan.create_regular.len(), 1, "local-only still created: {d:?}");
    assert!(d.pending_regular_deletes.is_empty(), "no base => no pending: {d:?}");
    assert!(d.plan.drop_regular.is_empty());
}

#[test]
fn three_way_id_index_filtered_on_all_sides() {
    let base = vec![ix("_id_", json!({"_id": 1}))];
    let local = vec![ix("_id_", json!({"_id": 1}))];
    let remote = vec![ix("_id_", json!({"_id": 1}))];
    let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
    assert!(d.plan.drop_regular.is_empty());
    assert!(d.plan.create_regular.is_empty());
    assert!(d.pending_regular_deletes.is_empty());
}

#[test]
fn three_way_v_only_diff_is_noop() {
    let base = vec![json!({"name": "ix_y", "key": {"y": 1}})];
    let local = vec![json!({"name": "ix_y", "key": {"y": 1}})];
    let remote = vec![json!({"name": "ix_y", "key": {"y": 1}, "v": 2})];
    let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
    assert!(d.plan.drop_regular.is_empty(), "{d:?}");
    assert!(d.plan.create_regular.is_empty(), "{d:?}");
    assert!(d.pending_regular_deletes.is_empty());
}

#[test]
fn three_way_search_user_removed_is_pending() {
    let s = |name: &str, dynamic: bool| json!({"name": name, "mappings": {"dynamic": dynamic}});
    let base = vec![s("sx_gone", true)];
    let local: Vec<serde_json::Value> = vec![];
    let remote = vec![s("sx_gone", true)];
    let d = diff_indexes_3way(&[], &base, &[], &local, &[], &remote);
    assert_eq!(d.pending_search_deletes, vec!["sx_gone".to_string()]);
    assert!(d.plan.drop_search.is_empty());
}

#[test]
fn three_way_search_admin_added_survives() {
    let s = |name: &str, dynamic: bool| json!({"name": name, "mappings": {"dynamic": dynamic}});
    let d = diff_indexes_3way(&[], &[], &[], &[], &[], &[s("sx_admin", true)]);
    assert!(d.pending_search_deletes.is_empty(), "{d:?}");
    assert!(d.plan.drop_search.is_empty());
}
```

- [ ] **Step 4: Run the tests to verify they fail to compile (function absent)**

Run: `cargo test --lib push::mdh::tests 2>&1 | tail -20`
Expected: compile error — `cannot find function diff_indexes_3way` / `cannot find type ThreeWayDiff`.

- [ ] **Step 5: Implement `ThreeWayDiff`, `diff_one_kind`, `diff_indexes_3way`**

Add to `src/cli/push/mdh.rs` (e.g. just below `diff_indexes`):

```rust
/// Result of the base-aware (3-way) within-env index diff. `plan` carries
/// creates and changed-definition drop+recreate pairs (always applied);
/// `pending_*_deletes` carries genuine user removals (in base, gone from
/// local, still on remote) which are GATED behind `--allow-deletes`.
#[derive(Debug, Default)]
pub(crate) struct ThreeWayDiff {
    pub plan: DiffPlan,
    pub pending_regular_deletes: Vec<String>,
    pub pending_search_deletes: Vec<String>,
}

/// Base-aware diff for the within-env push driver. Unlike `diff_indexes`
/// (2-way mirror), this distinguishes a *user removal* (index was in the
/// last-synced base, removed locally, still on remote) from an *admin
/// addition* (index appeared on remote, never in base or local). The former
/// is a gated pending delete; the latter is left untouched.
pub(crate) fn diff_indexes_3way(
    base_regular: &[Value],
    base_search: &[Value],
    local_regular: &[Value],
    local_search: &[Value],
    remote_regular: &[Value],
    remote_search: &[Value],
) -> ThreeWayDiff {
    let mut out = ThreeWayDiff::default();
    diff_one_kind(
        base_regular,
        local_regular,
        remote_regular,
        true, // filter the implicit _id_ regular index
        &mut out.plan.drop_regular,
        &mut out.plan.create_regular,
        &mut out.pending_regular_deletes,
    );
    diff_one_kind(
        base_search,
        local_search,
        remote_search,
        false,
        &mut out.plan.drop_search,
        &mut out.plan.create_search,
        &mut out.pending_search_deletes,
    );
    out
}

/// Core 3-way classification for one index kind (regular or search).
/// `drops`/`creates` receive always-applied changed-def recreate pairs and
/// local-only creates; `pending_deletes` receives gated user removals.
fn diff_one_kind(
    base: &[Value],
    local: &[Value],
    remote: &[Value],
    filter_id: bool,
    drops: &mut Vec<String>,
    creates: &mut Vec<Value>,
    pending_deletes: &mut Vec<String>,
) {
    let base_map = index_by_name(base, filter_id);
    let local_map = index_by_name(local, filter_id);
    let remote_map = index_by_name(remote, filter_id);

    // Creates + changed-def recreate, driven by local (BTreeMap → sorted,
    // deterministic).
    for (name, local_def) in &local_map {
        match remote_map.get(name) {
            None => creates.push((*local_def).clone()), // local-only
            Some(remote_def) => {
                if !defs_equivalent(local_def, remote_def) {
                    drops.push(name.clone());
                    creates.push((*local_def).clone());
                }
            }
        }
    }
    // Removals, driven by remote-only entries.
    for name in remote_map.keys() {
        if local_map.contains_key(name) {
            continue; // present locally → handled above
        }
        // Remote-only: a genuine user removal ONLY if it was in the base.
        // Not in base ⇒ admin-added ⇒ survive (never dropped).
        if base_map.contains_key(name) {
            pending_deletes.push(name.clone());
        }
    }
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib push::mdh::tests 2>&1 | tail -20`
Expected: all `push::mdh::tests` pass, including the existing `diff_*` tests and the new `three_way_*` tests.

- [ ] **Step 7: Confirm the whole lib still builds + clippy clean for the file**

Run: `cargo build 2>&1 | tail -5 && cargo clippy --lib 2>&1 | grep -i "mdh\|warning: unused" | head`
Expected: builds; no new warnings about `index_by_name` being unused or `ThreeWayDiff` dead (it's used by tests now; the driver wiring lands in Task 2 — if clippy flags `diff_indexes_3way`/`ThreeWayDiff` as unused at this point, that is expected and resolved by Task 2; do NOT add `#[allow(dead_code)]` — proceed).

- [ ] **Step 8: Commit**

```bash
git add src/model/index_set.rs src/cli/push/mdh.rs
git commit -m "feat(mdh): base-aware 3-way index diff (admin-added survives, user-removals pending)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Wire `push_dataset` to the safe diff + delete gate + base-cache write

**Files:**
- Modify: `src/cli/push/mdh.rs` (`push_dataset` signature + body; add `DeleteGate` + `classify_delete_gate` + `prompt_confirm_index_drops`; add gate unit tests)
- Modify: `src/cli/sync/execute.rs:3547-3556` (call-site: pass `ctx.paths`, `allow_deletes`, `ctx.interactive`)

**Interfaces:**
- Consumes: `diff_indexes_3way` / `ThreeWayDiff` (Task 1); `crate::state::base_cache::{read, write}`; `crate::state::{Lockfile, ObjectEntry, content_hash}` (already imported); `crate::cli::stdin_coord::read_line_coordinated`; `crate::paths::Paths`; `Log::with_prompt`.
- Produces:
  - New `push_dataset` signature:
    ```rust
    pub async fn push_dataset(
        client: &DataStorageClient,
        lockfile: &mut Lockfile,
        collection_name: &str,
        slug: &str,
        indexes_path: &Path,
        paths: &crate::paths::Paths,
        allow_deletes: bool,
        interactive: bool,
        progress: &Arc<Log>,
    ) -> Result<usize>
    ```
  - `pub(crate) enum DeleteGate { Proceed, Bail, Prompt }`
  - `pub(crate) fn classify_delete_gate(pending: usize, allow_deletes: bool, interactive: bool) -> DeleteGate`

- [ ] **Step 1: Write the failing unit tests for `classify_delete_gate`**

Add to `src/cli/push/mdh.rs` `mod tests`:

```rust
#[test]
fn gate_no_pending_is_proceed() {
    assert_eq!(classify_delete_gate(0, false, false), DeleteGate::Proceed);
    assert_eq!(classify_delete_gate(0, false, true), DeleteGate::Proceed);
}

#[test]
fn gate_allow_deletes_proceeds() {
    assert_eq!(classify_delete_gate(3, true, false), DeleteGate::Proceed);
    assert_eq!(classify_delete_gate(3, true, true), DeleteGate::Proceed);
}

#[test]
fn gate_noninteractive_without_flag_bails() {
    assert_eq!(classify_delete_gate(1, false, false), DeleteGate::Bail);
}

#[test]
fn gate_interactive_without_flag_prompts() {
    assert_eq!(classify_delete_gate(1, false, true), DeleteGate::Prompt);
}
```

`DeleteGate` must `#[derive(Debug, PartialEq)]` for these asserts.

- [ ] **Step 2: Run to verify failure (type/function absent)**

Run: `cargo test --lib push::mdh::tests::gate 2>&1 | tail -20`
Expected: compile error — `cannot find type DeleteGate` / `cannot find function classify_delete_gate`.

- [ ] **Step 3: Implement the gate classifier**

Add to `src/cli/push/mdh.rs` (module scope, near `diff_indexes_3way`):

```rust
/// Outcome of gating index-deletion (pure; mirrors the global delete gate
/// `crate::cli::push::deletes::confirm_or_refuse`).
#[derive(Debug, PartialEq)]
pub(crate) enum DeleteGate {
    /// Apply the pending deletes (nothing gated, or `--allow-deletes` set).
    Proceed,
    /// Non-interactive without `--allow-deletes`: refuse the destructive push.
    Bail,
    /// Interactive without `--allow-deletes`: caller must prompt [y/N].
    Prompt,
}

/// Decide how to treat `pending` user-removal index drops, mirroring rdc's
/// global delete gate: `--allow-deletes` ⇒ proceed; else non-TTY ⇒ bail;
/// else (TTY) ⇒ prompt. With nothing pending, "proceed" is a no-op.
pub(crate) fn classify_delete_gate(
    pending: usize,
    allow_deletes: bool,
    interactive: bool,
) -> DeleteGate {
    if pending == 0 || allow_deletes {
        return DeleteGate::Proceed;
    }
    if !interactive {
        return DeleteGate::Bail;
    }
    DeleteGate::Prompt
}
```

- [ ] **Step 4: Run to verify the gate tests pass**

Run: `cargo test --lib push::mdh::tests::gate 2>&1 | tail -20`
Expected: the four `gate_*` tests pass.

- [ ] **Step 5: Add the interactive-prompt helper**

Add to `src/cli/push/mdh.rs` (module scope). It clears the status line via
`progress.with_prompt` (same wrapper the object-delete phase uses) and reads
y/N through the stdin coordinator (same as `confirm_or_refuse`):

```rust
/// Interactive [y/N] confirmation for dropping remote MDH indexes that are no
/// longer present locally. Returns `true` to proceed with the drops.
fn prompt_confirm_index_drops(
    progress: &Arc<Log>,
    collection_name: &str,
    pending_regular: &[String],
    pending_search: &[String],
) -> Result<bool> {
    progress.with_prompt(|| -> Result<bool> {
        use std::io::Write;
        let n = pending_regular.len() + pending_search.len();
        eprintln!();
        eprintln!(
            "The following {n} MDH index(es) on '{collection_name}' would be DROPPED \
             (no longer present locally):"
        );
        for name in pending_regular {
            eprintln!("  - regular index '{name}'");
        }
        for name in pending_search {
            eprintln!("  - search index '{name}'");
        }
        eprint!("Proceed with the drop(s)? [y/N] ");
        std::io::stderr().flush().ok();
        let ans = crate::cli::stdin_coord::read_line_coordinated()?
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        Ok(ans == "y" || ans == "yes")
    })
}
```

- [ ] **Step 6: Rewrite the body of `push_dataset`**

Replace `push_dataset` (`src/cli/push/mdh.rs:54-110`) with the version below.
Changes from the current code: new `paths`/`allow_deletes`/`interactive`
params; read the base from the base cache; compute the 3-way diff; gate the
pending deletes; refresh the lockfile hash **and write the base cache** only on
a fully-applied push.

```rust
pub async fn push_dataset(
    client: &DataStorageClient,
    lockfile: &mut Lockfile,
    collection_name: &str,
    slug: &str,
    indexes_path: &Path,
    paths: &crate::paths::Paths,
    allow_deletes: bool,
    interactive: bool,
    progress: &Arc<Log>,
) -> Result<usize> {
    let local_raw = std::fs::read(indexes_path)
        .with_context(|| format!("reading {}", indexes_path.display()))?;
    let local_set: IndexSet = serde_json::from_slice(&local_raw)
        .with_context(|| format!("parsing {}", indexes_path.display()))?;

    // Base leg of the 3-way diff: the last-synced index set, written to the
    // base cache by the MDH pull driver. Absent (None) on a never-synced
    // dataset → empty set → no removals can be proven (strictly safe).
    let base_set: IndexSet = match crate::state::base_cache::read(paths, indexes_path)? {
        Some(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing base cache for {}", indexes_path.display()))?,
        None => IndexSet::default(),
    };

    // Fetch the live remote state directly so an admin's UI-added indexes are
    // visible to the diff (and, being absent from base, preserved).
    let remote_regular = client
        .list_indexes(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("listing regular indexes for '{collection_name}'"))?;
    let remote_search = client
        .list_search_indexes(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("listing search indexes for '{collection_name}'"))?;

    let diff = diff_indexes_3way(
        &base_set.regular,
        &base_set.search,
        &local_set.regular,
        &local_set.search,
        &remote_regular,
        &remote_search,
    );
    let mut plan = diff.plan;

    // Gate genuine user-removal drops behind --allow-deletes (mirrors the
    // global delete gate). Changed-def recreates in `plan` are NOT gated.
    let pending = diff.pending_regular_deletes.len() + diff.pending_search_deletes.len();
    let mut skipped = false;
    match classify_delete_gate(pending, allow_deletes, interactive) {
        DeleteGate::Proceed => {
            plan.drop_regular.extend(diff.pending_regular_deletes.iter().cloned());
            plan.drop_search.extend(diff.pending_search_deletes.iter().cloned());
        }
        DeleteGate::Bail => {
            anyhow::bail!(
                "{pending} MDH index(es) on '{collection_name}' marked for deletion but \
                 --allow-deletes was not passed. Re-run with --allow-deletes to authorise \
                 the destructive push, or restore {} to cancel.",
                indexes_path.display()
            );
        }
        DeleteGate::Prompt => {
            let proceed = prompt_confirm_index_drops(
                progress,
                collection_name,
                &diff.pending_regular_deletes,
                &diff.pending_search_deletes,
            )?;
            if proceed {
                plan.drop_regular.extend(diff.pending_regular_deletes.iter().cloned());
                plan.drop_search.extend(diff.pending_search_deletes.iter().cloned());
            } else {
                skipped = true;
                progress.event(
                    Action::Skip,
                    &format!("mdh/{slug} {pending} index deletion(s) skipped"),
                );
            }
        }
    }

    let ops = apply_diff(client, collection_name, slug, &plan, progress).await?;

    // Refresh the lockfile content_hash AND the base cache only when the push
    // fully reconciled remote to local (no skipped removals). Refreshing on a
    // skipped removal would make the next sync's `local_hash == base` gate skip
    // the dataset and silently forget the pending removal. Writing the base
    // cache here restores the cache↔lockfile hash invariant (the old code
    // refreshed the lockfile but never the base cache).
    let fully_applied = ops > 0 && !skipped;
    if fully_applied {
        let hash = content_hash(&local_raw, &crate::state::Lockfile::default());
        let map = lockfile
            .objects
            .entry("mdh_indexes".to_string())
            .or_default();
        map.insert(
            slug.to_string(),
            ObjectEntry {
                id: 0,
                modified_at: None,
                content_hash: Some(hash),
                secrets_hash: None,
            },
        );
        crate::state::base_cache::write(paths, indexes_path, &local_raw)
            .with_context(|| format!("writing base cache for mdh/{slug}"))?;
    }

    Ok(ops)
}
```

- [ ] **Step 7: Update the call site in `execute.rs`**

In `src/cli/sync/execute.rs`, replace the `push_dataset` call (`:3547-3556`):

```rust
                    crate::cli::push::mdh::push_dataset(
                        &catalog.mdh.client,
                        ctx.lockfile,
                        &collection.name,
                        slug,
                        &indexes_path,
                        ctx.paths,
                        allow_deletes,
                        ctx.interactive,
                        progress,
                    )
                    .await
                    .with_context(|| format!("pushing local index edits for mdh/{slug}"))?;
```

(`ctx.paths` is `&Paths`, `allow_deletes` is the fn param at `execute.rs:3079`,
`ctx.interactive` is on `PullCtx` — both already in scope here.)

- [ ] **Step 8: Build, run the full mdh suite + a broad check**

Run: `cargo build 2>&1 | tail -5 && cargo test --lib push::mdh 2>&1 | tail -15`
Expected: builds clean; all `push::mdh` unit tests pass (existing `diff_*`, new `three_way_*` and `gate_*`).

- [ ] **Step 9: Confirm no other caller broke + clippy**

Run: `cargo test --lib 2>&1 | tail -8 && cargo clippy --lib 2>&1 | tail -8`
Expected: full lib test suite green; clippy reports no new warnings (the Task-1 "possibly unused" note is now resolved — `diff_indexes_3way`/`ThreeWayDiff` are used by `push_dataset`).

- [ ] **Step 10: Commit**

```bash
git add src/cli/push/mdh.rs src/cli/sync/execute.rs
git commit -m "feat(mdh): safe within-env push — base-aware drops gated by --allow-deletes

push_dataset now diffs against the base cache, never drops admin-added
remote-only indexes, gates genuine user-removals behind --allow-deletes
(bail non-TTY / prompt TTY), and writes the base cache on a fully-applied
push (restoring the cache<->lockfile hash invariant).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Test-only raw Data-Storage helper for MDH live coverage

**Files:**
- Create: `tests/live/support/mdh.rs`
- Modify: `tests/live/support/mod.rs` (add `pub mod mdh;`)

**Interfaces:**
- Consumes: `crate::support::config::LiveConfig`; `crate::support::run_id::RunId`; `rdc::api::DataStorageClient`; `rdc::config::EnvConfig`; `reqwest`, `serde_json`, `anyhow`.
- Produces:
  - `pub fn mdh_collection_name(run_id: &RunId) -> String` → `rdc_it_<id>_mdh` (Mongo-safe; embeds the run id).
  - `pub const MDH_COLLECTION_MARKER: &str = "rdc_it_";`
  - `pub struct MdhRaw { /* reqwest client + bearer token + ds_base */ }` with:
    - `pub fn connect(cfg: &LiveConfig) -> anyhow::Result<MdhRaw>`
    - `pub fn ds_client(&self) -> rdc::api::DataStorageClient` (a fresh rdc client over the same base/token, for lists + index create/drop)
    - `pub async fn create_collection(&self, name: &str) -> anyhow::Result<()>`
    - `pub async fn insert_one(&self, name: &str, doc: serde_json::Value) -> anyhow::Result<()>`
    - `pub async fn drop_collection(&self, name: &str) -> anyhow::Result<()>`
    - `pub async fn list_collection_names(&self) -> anyhow::Result<Vec<String>>`
    - `pub async fn wait_for_regular_index(&self, coll: &str, name: &str, present: bool) -> anyhow::Result<()>` (poll until the named index appears/disappears, ~30s bound)
    - `pub async fn wait_for_search_index(&self, coll: &str, name: &str, present: bool) -> anyhow::Result<()>` (same, ~90s bound — Atlas Search create/drop is slower)
    - `pub async fn try_create_search_index(&self, coll: &str, name: &str) -> anyhow::Result<bool>` (returns `Ok(false)` when Atlas Search is unsupported on the cluster, so the scenario can gracefully skip the search sub-phase rather than fail)

- [ ] **Step 1: Write the failing hermetic unit test for the pure helper**

Create `tests/live/support/mdh.rs` containing only the test first (to drive the
helper), or add the test alongside the impl in Step 2. The test (runs under
default `cargo test --test live`, no network):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::run_id::RunId;

    #[test]
    fn collection_name_is_mongo_safe_and_marked() {
        let id = RunId::new();
        let name = mdh_collection_name(&id);
        assert!(name.starts_with(MDH_COLLECTION_MARKER), "{name}");
        assert!(name.contains(id.as_str()), "{name}");
        // Mongo-safe: lowercase alnum + underscore only (no hyphens/spaces).
        assert!(
            name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "collection name not mongo-safe: {name}"
        );
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test live support::mdh::tests 2>&1 | tail -20`
Expected: compile error — module `mdh` not declared / `mdh_collection_name` absent.

- [ ] **Step 3: Implement the helper**

Write `tests/live/support/mdh.rs` (above the `#[cfg(test)] mod tests` block):

```rust
use crate::support::config::LiveConfig;
use crate::support::run_id::RunId;
use anyhow::{anyhow, Context, Result};
use rdc::api::DataStorageClient;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Stable prefix shared by every throwaway MDH collection this harness
/// creates. Mongo-safe (underscores), distinct from the core-API `rdc-it-`
/// marker because collection names use a different charset convention.
pub const MDH_COLLECTION_MARKER: &str = "rdc_it_";

/// Per-run throwaway collection name: `rdc_it_<id>_mdh`. The run id is base36
/// alnum, so the whole name is Mongo-safe.
pub fn mdh_collection_name(run_id: &RunId) -> String {
    format!("{}{}_mdh", MDH_COLLECTION_MARKER, run_id.as_str())
}

/// Raw Data-Storage client for the 3 collection-lifecycle endpoints rdc's
/// `DataStorageClient` does not expose. Everything else (index create/drop,
/// listing) reuses `DataStorageClient` via [`MdhRaw::ds_client`].
#[allow(dead_code)]
pub struct MdhRaw {
    http: reqwest::Client,
    base: String,
    token: String,
}

#[allow(dead_code)]
impl MdhRaw {
    pub fn connect(cfg: &LiveConfig) -> Result<MdhRaw> {
        // Derive the Data-Storage base the same way rdc does (single source of
        // truth), via the already-public EnvConfig helper.
        let base = rdc::config::EnvConfig {
            api_base: cfg.api_base.clone(),
            org_id: cfg.org_id,
        }
        .data_storage_base();
        let http = reqwest::Client::builder()
            .build()
            .context("building reqwest client for MDH raw helper")?;
        Ok(MdhRaw { http, base, token: cfg.token.clone() })
    }

    /// A fresh rdc DataStorageClient over the same base + token, for index
    /// create/drop + listing in tests.
    pub fn ds_client(&self) -> DataStorageClient {
        DataStorageClient::new(self.base.clone(), self.token.clone())
            .expect("construct DataStorageClient")
    }

    async fn post(&self, path: &str, body: Value) -> Result<(reqwest::StatusCode, String)> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    pub async fn create_collection(&self, name: &str) -> Result<()> {
        let (status, body) = self
            .post("/v1/collections/create", json!({ "collectionName": name }))
            .await?;
        if !status.is_success() {
            return Err(anyhow!("create_collection {name}: {status} {body}"));
        }
        Ok(())
    }

    pub async fn insert_one(&self, name: &str, doc: Value) -> Result<()> {
        let (status, body) = self
            .post(
                "/v1/data/insert_one",
                json!({ "collectionName": name, "document": doc }),
            )
            .await?;
        if !status.is_success() {
            return Err(anyhow!("insert_one {name}: {status} {body}"));
        }
        Ok(())
    }

    /// Drop a collection. Async (202); best-effort — a missing collection is
    /// not an error (idempotent teardown).
    pub async fn drop_collection(&self, name: &str) -> Result<()> {
        let (status, body) = self
            .post("/v1/collections/drop", json!({ "collectionName": name }))
            .await?;
        // 2xx (incl. 202) = accepted; 404 / "not found" = already gone.
        if status.is_success() || status.as_u16() == 404 || body.contains("not found") {
            return Ok(());
        }
        Err(anyhow!("drop_collection {name}: {status} {body}"))
    }

    pub async fn list_collection_names(&self) -> Result<Vec<String>> {
        let client = self.ds_client();
        let cols = client.list_collections(None).await?;
        Ok(cols.into_iter().map(|c| c.name).collect())
    }

    /// Poll until the named regular index is present (`present=true`) or absent
    /// (`present=false`). Bounded at 30s — index create/drop is async (202).
    pub async fn wait_for_regular_index(&self, coll: &str, name: &str, present: bool) -> Result<()> {
        self.wait_for_index(coll, name, present, false, Duration::from_secs(30)).await
    }

    /// Same as `wait_for_regular_index` but for Atlas Search indexes, with a
    /// longer bound (Atlas create/drop runs in the background, up to ~60s).
    pub async fn wait_for_search_index(&self, coll: &str, name: &str, present: bool) -> Result<()> {
        self.wait_for_index(coll, name, present, true, Duration::from_secs(90)).await
    }

    async fn wait_for_index(
        &self,
        coll: &str,
        name: &str,
        present: bool,
        search: bool,
        bound: Duration,
    ) -> Result<()> {
        let client = self.ds_client();
        let start = Instant::now();
        loop {
            let list = if search {
                client.list_search_indexes(coll, None).await?
            } else {
                client.list_indexes(coll, None).await?
            };
            let found = list
                .iter()
                .any(|ix| ix.get("name").and_then(|n| n.as_str()) == Some(name));
            if found == present {
                return Ok(());
            }
            if start.elapsed() >= bound {
                return Err(anyhow!(
                    "timed out waiting for {} index '{name}' on '{coll}' to be present={present}",
                    if search { "search" } else { "regular" }
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Attempt to create an Atlas Search index out-of-band. Returns `Ok(true)`
    /// on success, `Ok(false)` when the cluster does not support Search (so the
    /// caller can skip the search sub-phase instead of failing). Other errors
    /// propagate.
    pub async fn try_create_search_index(&self, coll: &str, name: &str) -> Result<bool> {
        let client = self.ds_client();
        match client
            .create_search_index(coll, name, &json!({ "dynamic": true }), &json!([]), None)
            .await
        {
            Ok(()) => Ok(true),
            Err(e) => {
                let msg = format!("{e:#}").to_lowercase();
                // Treat "not supported / not enabled / 404 / 501" as "no Search
                // on this cluster" → graceful skip; anything else is a real error.
                if msg.contains("not support")
                    || msg.contains("not enabled")
                    || msg.contains("404")
                    || msg.contains("501")
                    || msg.contains("unavailable")
                {
                    eprintln!("MDH: Atlas Search unsupported on this cluster, skipping search sub-phase ({e:#})");
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }
}
```

Note: confirm `rdc::model::Collection` exposes a `name: String` field (used by
`list_collection_names`). It does — `Collection` is what
`DataStorageClient::list_collections` returns (`src/api/data_storage.rs:60`);
if the field is named differently, adapt the `.map(|c| c.name)` accessor.

- [ ] **Step 4: Declare the module**

In `tests/live/support/mod.rs`, add after the existing `pub mod` lines:

```rust
pub mod mdh;
```

- [ ] **Step 5: Run the hermetic unit test + ensure the live bin compiles**

Run: `cargo test --test live support::mdh::tests 2>&1 | tail -20`
Expected: `collection_name_is_mongo_safe_and_marked` passes; the `live` test binary compiles (the `#[ignore]` scenarios still don't run).

- [ ] **Step 6: Commit**

```bash
git add tests/live/support/mdh.rs tests/live/support/mod.rs
git commit -m "test(live): raw Data-Storage helper for MDH coverage (collection lifecycle)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Teardown + janitor sweep for throwaway MDH collections

**Files:**
- Modify: `tests/live/support/teardown.rs` (`teardown_by_prefix` also drops MDH collections; `Teardown` holds a `LiveConfig` so it can connect an `MdhRaw` at drop time)
- Modify: `tests/live/scenarios/janitor.rs` (assert no `rdc_it_*` collections remain after the sweep)

**Interfaces:**
- Consumes: `crate::support::mdh::{MdhRaw, MDH_COLLECTION_MARKER, mdh_collection_name}` (Task 3); `crate::support::config::LiveConfig`.
- Produces:
  - `pub async fn drop_mdh_collections_by_prefix(cfg: &LiveConfig, marker: &str) -> anyhow::Result<()>` (drops every collection whose name starts with `marker`).
  - `Teardown::new` signature gains a `LiveConfig` (so drop-time can build an `MdhRaw`). Update the one call site (`round_trip.rs:25`) is **not** needed yet — but Task 5's scenario uses the new signature. To avoid breaking `round_trip.rs`, keep `Teardown::new(client, run_id)` and add a separate `Teardown::with_mdh(client, run_id, cfg)` constructor, OR add an optional `cfg: Option<LiveConfig>` — see Step 1 for the exact, non-breaking approach.

- [ ] **Step 1: Add a non-breaking MDH-aware teardown path**

In `tests/live/support/teardown.rs`, add a free function and extend `Teardown`
**without changing the existing `Teardown::new(client, run_id)` signature** (so
`round_trip.rs` and the other scenarios keep compiling). Add an optional config
field defaulting to `None`:

Add `use crate::support::config::LiveConfig;` to the top of `teardown.rs`
(alongside the existing `use` lines). `MdhRaw` is referenced by full path
(`crate::support::mdh::MdhRaw`), so no extra import is needed. Then add:

```rust
/// Drop every MDH collection whose name starts with `marker` (the throwaway
/// `rdc_it_*` collections this harness creates). Best-effort; async 202 drops.
pub async fn drop_mdh_collections_by_prefix(cfg: &LiveConfig, marker: &str) -> anyhow::Result<()> {
    let raw = crate::support::mdh::MdhRaw::connect(cfg)?;
    let names = match raw.list_collection_names().await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("teardown(mdh): list collections failed (continuing): {e:#}");
            return Ok(());
        }
    };
    for name in names.into_iter().filter(|n| n.starts_with(marker)) {
        if let Err(e) = raw.drop_collection(&name).await {
            eprintln!("teardown(mdh): drop collection {name} failed (continuing): {e:#}");
        }
    }
    Ok(())
}
```

Extend `Teardown` with an optional `cfg`:

```rust
#[allow(dead_code)]
pub struct Teardown {
    client: LiveClient,
    run_id: RunId,
    cfg: Option<LiveConfig>,
}

#[allow(dead_code)]
impl Teardown {
    pub fn new(client: LiveClient, run_id: RunId) -> Teardown {
        Teardown { client, run_id, cfg: None }
    }
    /// Like `new`, but also drops this run's throwaway MDH collection(s) on drop.
    pub fn with_mdh(client: LiveClient, run_id: RunId, cfg: LiveConfig) -> Teardown {
        Teardown { client, run_id, cfg: Some(cfg) }
    }
    pub fn client(&self) -> &LiveClient { &self.client }
    pub fn run_id(&self) -> &RunId { &self.run_id }
}
```

In `impl Drop for Teardown`, inside the existing dedicated-OS-thread block,
after the `teardown_by_prefix` call, add the MDH drop when `cfg` is present
(use the run-specific collection name so we never touch another run's
collections):

```rust
                if let Err(e) = rt.block_on(teardown_by_prefix(client, &prefix)) {
                    eprintln!("teardown: {e:#}");
                }
                if let Some(cfg) = cfg {
                    // Drop only THIS run's throwaway collection.
                    let coll = crate::support::mdh::mdh_collection_name(run_id);
                    if let Ok(raw) = crate::support::mdh::MdhRaw::connect(cfg) {
                        if let Err(e) = rt.block_on(raw.drop_collection(&coll)) {
                            eprintln!("teardown(mdh): drop {coll} failed (continuing): {e:#}");
                        }
                    }
                }
```

To borrow `cfg` and `run_id` inside the scoped thread, bind them before the
`thread::scope` call (alongside the existing `let prefix = ...; let client = ...;`):

```rust
        let prefix = self.run_id.list_prefix();
        let client = &self.client;
        let run_id = &self.run_id;
        let cfg = self.cfg.as_ref();
```

- [ ] **Step 2: Extend the janitor scenario to sweep + assert MDH collections**

In `tests/live/scenarios/janitor.rs`, after the existing core sweep + asserts,
add an MDH sweep + assertion:

```rust
    // MDH: drop every throwaway `rdc_it_*` collection a crashed run left behind.
    crate::support::teardown::drop_mdh_collections_by_prefix(
        &cfg,
        crate::support::mdh::MDH_COLLECTION_MARKER,
    )
    .await
    .expect("janitor mdh sweep");
    // Collection drop is async (202); poll until none remain (bounded).
    let raw = crate::support::mdh::MdhRaw::connect(&cfg).expect("connect mdh");
    let mut remaining = raw.list_collection_names().await.unwrap_or_default();
    let mut waited = 0;
    while remaining
        .iter()
        .any(|n| n.starts_with(crate::support::mdh::MDH_COLLECTION_MARKER))
        && waited < 30
    {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        remaining = raw.list_collection_names().await.unwrap_or_default();
        waited += 1;
    }
    let leftover: Vec<_> = remaining
        .into_iter()
        .filter(|n| n.starts_with(crate::support::mdh::MDH_COLLECTION_MARKER))
        .collect();
    assert!(leftover.is_empty(), "janitor left MDH collections: {leftover:?}");
```

(The janitor test already binds `cfg` at the top via `LiveConfig::from_env`.)

- [ ] **Step 3: Build the live test binary**

Run: `cargo test --test live 2>&1 | tail -15`
Expected: the `live` binary compiles; hermetic unit tests pass; `#[ignore]`
scenarios (including `live_janitor_sweep`) are listed but not run.

- [ ] **Step 4: Commit**

```bash
git add tests/live/support/teardown.rs tests/live/scenarios/janitor.rs
git commit -m "test(live): teardown + janitor drop throwaway rdc_it_* MDH collections

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: MDH live scenario (6 adversarial phases)

**Files:**
- Create: `tests/live/scenarios/mdh.rs`
- Modify: `tests/live/scenarios/mod.rs` (add `mod mdh;`)

**Interfaces:**
- Consumes: `crate::support::{config::LiveConfig, run_id::RunId, project::ProjectFixture, client::LiveClient, teardown::Teardown}`; `crate::support::mdh::{MdhRaw, mdh_collection_name}`; `crate::support::assert_local::load_lockfile` + `lockfile_keys` (to discover the dataset slug).
- Produces: `#[ignore]` test `live_mdh_index_lifecycle`.

**Slug discovery:** the dataset slug is `slugify(collection_name)` and is not
hardcoded — find it from the lockfile by matching the run id:
`lockfile_keys(&lf, "mdh_indexes").into_iter().find(|s| s.contains(run_id.as_str()))`.
The on-disk path is `envs/test/mdh/<slug>/indexes.json`.

- [ ] **Step 1: Write the scenario**

Create `tests/live/scenarios/mdh.rs`:

```rust
use crate::support::assert_local::{load_lockfile, lockfile_keys};
use crate::support::client::LiveClient;
use crate::support::config::LiveConfig;
use crate::support::mdh::{mdh_collection_name, MdhRaw};
use crate::support::project::ProjectFixture;
use crate::support::run_id::RunId;
use crate::support::teardown::Teardown;
use serde_json::json;

/// Read the local indexes.json for the run's dataset; returns (slug, value).
fn read_indexes(project: &ProjectFixture, run_id: &RunId) -> (String, serde_json::Value) {
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let slug = lockfile_keys(&lf, "mdh_indexes")
        .into_iter()
        .find(|s| s.contains(run_id.as_str()))
        .expect("an mdh_indexes slug for this run");
    let rel = format!("envs/test/mdh/{slug}/indexes.json");
    let v: serde_json::Value =
        serde_json::from_str(&project.read_to_string(&rel).expect("indexes.json on disk")).unwrap();
    (slug, v)
}

fn write_indexes(project: &ProjectFixture, slug: &str, v: &serde_json::Value) {
    let rel = format!("envs/test/mdh/{slug}/indexes.json");
    std::fs::write(project.path().join(&rel), serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

fn regular_names(v: &serde_json::Value) -> Vec<String> {
    v.get("regular")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|ix| ix.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

async fn remote_regular_names(raw: &MdhRaw, coll: &str) -> Vec<String> {
    raw.ds_client()
        .list_indexes(coll, None)
        .await
        .expect("list remote indexes")
        .iter()
        .filter_map(|ix| ix.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

/// Full MDH index lifecycle on a per-run throwaway collection: pull round-trip,
/// push create, push modify, gated safe-delete, admin-added survives,
/// idempotent re-sync. Never touches a real collection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_mdh_index_lifecycle() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let coll = mdh_collection_name(&run_id);
    let raw = MdhRaw::connect(&cfg).expect("connect mdh");

    // Teardown FIRST (drops the collection on any panic).
    let teardown = Teardown::with_mdh(
        LiveClient::connect(&cfg).expect("connect (teardown)"),
        run_id.clone(),
        cfg.clone(),
    );

    // --- seed remote out-of-band: collection + a doc + one regular index ---
    raw.create_collection(&coll).await.expect("create collection");
    raw.insert_one(&coll, json!({ "k": "v" })).await.expect("seed doc");
    raw.ds_client()
        .create_index(&coll, "ix_a", &json!({ "a": 1 }), &json!({}), None)
        .await
        .expect("seed ix_a");
    raw.wait_for_regular_index(&coll, "ix_a", true).await.expect("ix_a present");
    // Optionally seed an Atlas Search index (gracefully skipped if the cluster
    // has no Search support — `has_search` gates every search assertion below).
    let has_search = raw.try_create_search_index(&coll, "sx_a").await.expect("try search");
    if has_search {
        raw.wait_for_search_index(&coll, "sx_a", true).await.expect("sx_a present");
    }

    // --- Phase 1: pull round-trip ---
    let project = ProjectFixture::init(&cfg, &["test", "prod"]).expect("init project");
    let out = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(out.status.success(), "pull failed: {}", String::from_utf8_lossy(&out.stderr));
    let (slug, idx) = read_indexes(&project, &run_id);
    let names = regular_names(&idx);
    assert!(names.contains(&"ix_a".to_string()), "pulled regular names: {names:?}");
    assert!(!names.contains(&"_id_".to_string()), "_id_ must be stripped: {names:?}");
    if has_search {
        let search = idx.get("search").and_then(|s| s.as_array()).cloned().unwrap_or_default();
        let sx = search
            .iter()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("sx_a"))
            .expect("pulled search index sx_a");
        assert!(sx.get("mappings").is_some(), "search index must carry mappings: {sx:?}");
        // Server-managed fields must be stripped at pull time (normalized shape).
        for junk in ["id", "status", "queryable", "latestDefinition"] {
            assert!(sx.get(junk).is_none(), "server field '{junk}' must be stripped: {sx:?}");
        }
    }

    // --- Phase 2: push create (add ix_b locally) ---
    let mut idx2 = idx.clone();
    idx2["regular"].as_array_mut().unwrap().push(json!({ "name": "ix_b", "key": { "b": -1 } }));
    write_indexes(&project, &slug, &idx2);
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "push-create failed: {}", String::from_utf8_lossy(&out.stderr));
    raw.wait_for_regular_index(&coll, "ix_b", true).await.expect("ix_b created");
    // Idempotent: a second sync makes no further changes.
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "re-sync after create failed: {}", String::from_utf8_lossy(&out.stderr));

    // --- Phase 3: push modify (change ix_b's key) ---
    let (slug, mut idx3) = read_indexes(&project, &run_id);
    for ix in idx3["regular"].as_array_mut().unwrap() {
        if ix.get("name").and_then(|n| n.as_str()) == Some("ix_b") {
            ix["key"] = json!({ "b": 1 }); // -1 -> 1
        }
    }
    write_indexes(&project, &slug, &idx3);
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "push-modify failed: {}", String::from_utf8_lossy(&out.stderr));
    // After drop+recreate, ix_b exists with the new key.
    raw.wait_for_regular_index(&coll, "ix_b", true).await.expect("ix_b present after modify");
    let remote = raw.ds_client().list_indexes(&coll, None).await.expect("list");
    let ix_b = remote.iter().find(|ix| ix.get("name").and_then(|n| n.as_str()) == Some("ix_b")).expect("ix_b");
    assert_eq!(ix_b.get("key"), Some(&json!({ "b": 1 })), "ix_b key not updated: {ix_b:?}");

    // --- Phase 4: gated safe-delete (remove ix_b locally) ---
    let (slug, mut idx4) = read_indexes(&project, &run_id);
    idx4["regular"]
        .as_array_mut()
        .unwrap()
        .retain(|ix| ix.get("name").and_then(|n| n.as_str()) != Some("ix_b"));
    write_indexes(&project, &slug, &idx4);
    // Without --allow-deletes (non-interactive): must REFUSE and leave ix_b.
    let out = project.run_rdc(&["sync", "test"]);
    assert!(!out.status.success(), "sync without --allow-deletes should fail on a removal");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("allow-deletes"), "expected allow-deletes refusal: {stderr}");
    assert!(
        remote_regular_names(&raw, &coll).await.contains(&"ix_b".to_string()),
        "ix_b must survive the refused delete"
    );
    // With --allow-deletes: ix_b is dropped.
    let out = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "push-delete failed: {}", String::from_utf8_lossy(&out.stderr));
    raw.wait_for_regular_index(&coll, "ix_b", false).await.expect("ix_b dropped");

    // --- Phase 5: admin-added survives ---
    // Out-of-band create ix_admin (not in base, not in local). Make an
    // unrelated local change so the dataset is dirty, then sync --allow-deletes.
    raw.ds_client()
        .create_index(&coll, "ix_admin", &json!({ "z": 1 }), &json!({}), None)
        .await
        .expect("create ix_admin");
    raw.wait_for_regular_index(&coll, "ix_admin", true).await.expect("ix_admin present");
    let (slug, mut idx5) = read_indexes(&project, &run_id);
    idx5["regular"].as_array_mut().unwrap().push(json!({ "name": "ix_c", "key": { "c": 1 } }));
    write_indexes(&project, &slug, &idx5);
    let out = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "admin-added sync failed: {}", String::from_utf8_lossy(&out.stderr));
    raw.wait_for_regular_index(&coll, "ix_c", true).await.expect("ix_c created");
    let after = remote_regular_names(&raw, &coll).await;
    assert!(after.contains(&"ix_admin".to_string()), "admin-added index must survive: {after:?}");

    // --- Phase 6: idempotency (final re-sync = no error, stable) ---
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "final re-sync failed: {}", String::from_utf8_lossy(&out.stderr));

    // Search index (if seeded) must have survived every sync — a broken
    // normalize/equivalence check would have drop+recreated it each cycle.
    if has_search {
        let search_names: Vec<String> = raw
            .ds_client()
            .list_search_indexes(&coll, None)
            .await
            .expect("list search")
            .iter()
            .filter_map(|ix| ix.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        assert!(search_names.contains(&"sx_a".to_string()), "sx_a must survive: {search_names:?}");
    }

    drop(teardown);
}
```

- [ ] **Step 2: Register the scenario module**

In `tests/live/scenarios/mod.rs`, add (next to the other `mod` lines):

```rust
mod mdh;
```

- [ ] **Step 3: Build the live binary (compile gate)**

Run: `cargo test --test live 2>&1 | tail -15`
Expected: compiles; hermetic unit tests pass; `live_mdh_index_lifecycle` is
listed as ignored. (Live execution is the controller's verification step,
Task 6 — a subagent without credentials cannot run it.)

- [ ] **Step 4: Verify `cfg.clone()` / `LiveConfig: Clone`**

`LiveConfig` derives `Clone` (`config.rs:13`), so `cfg.clone()` for
`Teardown::with_mdh` compiles. If `ProjectFixture::read_to_string` returns
`Option<String>` (it does — `project.rs:66`), the `.expect(...)` in
`read_indexes` is correct.

- [ ] **Step 5: Commit**

```bash
git add tests/live/scenarios/mdh.rs tests/live/scenarios/mod.rs
git commit -m "test(live): MDH index lifecycle scenario (pull/create/modify/gated-delete/admin-survives)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Live verification + docs (controller-run)

**Files:**
- Modify: `README.md` (the "Live integration testing" section — note MDH coverage + that the throwaway collection is dropped on teardown)
- (No code) Live run on the sandbox.

**Interfaces:** none (verification + docs).

This task is run by the controller (it needs `RDC_LIVE_*` with the secret
sandbox token, which never enters the repo or a subagent prompt).

- [ ] **Step 1: Run the MDH scenario live (and the janitor) against the sandbox**

With `RDC_LIVE_API_BASE` / `RDC_LIVE_ORG_ID` / `RDC_LIVE_TOKEN` exported:

Run: `cargo test --test live live_mdh_index_lifecycle -- --ignored --test-threads=1 2>&1 | tail -40`
Expected: PASS. Then run the janitor: `cargo test --test live live_janitor_sweep -- --ignored --test-threads=1 2>&1 | tail -20` → PASS, no `rdc_it_*` collection left.

- [ ] **Step 2: Run the full live suite (regression)**

Run: `cargo test --test live -- --ignored --test-threads=1 2>&1 | tail -40`
Expected: all scenarios green (the 7 pre-existing + the new MDH one).

- [ ] **Step 3: Confirm the default suite is still green**

Run: `cargo test 2>&1 | tail -15`
Expected: full default `cargo test` passes (lib + all integration bins; live `#[ignore]` skipped).

- [ ] **Step 4: Document MDH live coverage in the README**

In `README.md`'s live-integration-testing section, add a sentence noting that
MDH index coverage runs on a per-run throwaway collection (`rdc_it_<id>_mdh`),
is dropped on teardown, and that within-env MDH push gates index deletions
behind `--allow-deletes` (admin-added remote indexes are preserved). Keep it
generic — no real collection names, host, or org id.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs: note MDH live coverage + --allow-deletes index-drop gating

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- Spec §3.1 (3-way diff, no-base fallback, `_id_` filter) → Task 1.
- Spec §3.2 (push_dataset gate + base-cache write + lockfile-refresh-on-full-apply) → Task 2.
- Spec §3.3 (call-site wiring) → Task 2 Step 7.
- Spec §3.4 (`IndexSet: Default`; no `pub` derivation change) → Task 1 Step 1; Task 3 uses `EnvConfig::data_storage_base()`.
- Spec §3.6 (hermetic unit tests for the pure diff) → Task 1 Steps 3-6; gate classifier → Task 2 Steps 1-4.
- Spec §4.1 (isolation/idempotency), §4.2 (harness additions) → Tasks 3, 4.
- Spec §4.3 (6 scenario phases, incl. regular + search indexes), §4.4 (the normalized search-index shape is asserted **structurally** — `mappings` present, server-managed fields stripped — rather than via a brittle serialized golden) → Task 5. Search coverage is gated on `has_search` so clusters without Atlas Search skip it gracefully (verify-first: the plan does not assume Search is enabled).
- Spec §5 (constraints) → Global Constraints + Task 6 verification.
- Spec §6 risks (per-dataset gating, mid-loop bail, search async, no `pub` change) → reflected in Task 2 (bail) and Task 3/5 (polling).

**Placeholder scan:** no TBD/TODO; every code step shows complete code; the one illustrative-import caveat in Task 4 Step 1 is explicitly flagged as "do not add."

**Type consistency:** `diff_indexes_3way` / `ThreeWayDiff` / `DeleteGate` / `classify_delete_gate` / `prompt_confirm_index_drops` signatures match between Task 1, Task 2, and their call sites. `push_dataset`'s new signature (Task 2) matches the call site (Task 2 Step 7). `mdh_collection_name` / `MdhRaw` / `MDH_COLLECTION_MARKER` (Task 3) match their uses in Tasks 4 and 5. `Teardown::with_mdh` (Task 4) matches its use (Task 5).

**Note on §4.4:** the scenario asserts structural facts (index present/absent, key updated, refusal message, search `mappings` present + server fields stripped) rather than pinning a serialized golden, so no `testdata/live/expected/mdh.toml` is introduced — this avoids a brittle golden for async/normalized output. The create/modify/gated-delete/admin-survives cycle runs on **regular** indexes (fast, deterministic 202s); the **search** index is seeded once and asserted for normalized-shape on pull and survival across every sync (the no-spurious-churn check), guarded by `has_search` so a cluster without Atlas Search skips it. If a maintainer later wants the exact on-disk normalized search shape pinned, add a capture-then-pin golden then.
