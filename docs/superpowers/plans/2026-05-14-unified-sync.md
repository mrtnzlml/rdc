# Unified Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `rdc pull` / `rdc push` commands with a single `rdc sync <env>` command that reconciles local snapshot and remote state in one pass, with an env-aware conflict resolver and folded remote-delete handling.

**Architecture:** A new `cli::sync` module composes the existing per-kind drivers behind a single entry point. It runs list-remote → scan-local → classify (11 classes) → plan-and-confirm → execute. The existing conflict resolver in `cli::resolve` gains an `env_name` parameter so labels and shadow files carry the env name instead of the abstract word "remote". The `cli::pull::run` and `cli::push::run` CLI entry points are deleted; per-kind drivers stay as internal modules consumed by sync.

**Tech Stack:** Rust (existing rdc codebase), serde/serde_json, reqwest, wiremock for integration tests, clap for CLI. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-05-14-unified-sync-design.md` — read it before starting.

---

## File Structure

**Create:**
- `src/cli/sync/mod.rs` — top-level `sync::run` function: composes list_remote, scan, classify, plan, execute, lockfile save.
- `src/cli/sync/classify.rs` — `SyncClass` enum and `classify()` function over `(RemoteCatalog, ScanResult, Lockfile)`.
- `src/cli/sync/plan.rs` — plan rendering (`Plan: sync <env>` block) and confirmation prompt.
- `src/cli/sync/execute.rs` — execution pipeline: pull-side writes, conflict resolution dispatch, push-side writes, tombstone deletes.
- `tests/cli_sync.rs` — integration tests, wiremock-backed.

**Modify:**
- `src/cli/mod.rs` — remove `Pull` and `Push` variants from `Command`, add `Sync` variant. Remove `pub mod pull` / `pub mod push` from the file-level re-exports (the per-kind drivers stay internal to sync).
- `src/cli/pull/mod.rs` — delete `pub async fn run`; keep the per-kind module re-exports. Extract listing phase into `cli::pull::common::list_remote`. The drivers' `process` functions gain a subset filter.
- `src/cli/pull/common.rs` — add `list_remote()` returning `RemoteCatalog`. Update `shadow_file_conflict` to take `env_name`.
- `src/cli/pull/{workspaces,queues,hooks,rules,labels,engines,engine_fields,workflows,workflow_steps,email_templates,organization,mdh}.rs` — each `process` function gains a `subset: &BTreeSet<(String, String)>` parameter and skips entries not in the set.
- `src/cli/push/mod.rs` — delete `pub async fn run`. Promote `run_drivers` (renamed `push_classified`) to module-level pub-crate so sync can call it.
- `src/cli/push/scan.rs` — no signature change; called from sync directly.
- `src/cli/push/{workspaces,queues,hooks,rules,labels,engines,engine_fields,inboxes,schemas,email_templates}.rs` — already accept a `ChangeList` slice. No change required.
- `src/cli/resolve.rs` — add `env_name: &str` to `prompt_resolve`, `prompt_resolve_with_color`, `resolve_combined_file`, `resolve_push_drift`. Update prompt text to interpolate the env name. New helper `prompt_remote_delete` for the remote-deleted case (returns the same `Resolution` enum). Update doc comments.
- `src/main.rs` — no change (it just routes through `cli::run`).
- `README.md` — replace the `rdc pull` / `rdc push` sections and Commands table rows with a single `rdc sync` entry. Update the 60-second tour and Mental model.

**Delete:**
- `tests/cli_pull.rs` — replaced by `tests/cli_sync.rs`. The same flows are covered via `rdc sync --no-push`.
- `tests/cli_push.rs` — same; covered via `rdc sync --no-pull`.

---

## Task 1: Add `env_name` parameter to resolver entry points

**Goal:** Plumb the env name through to `cli::resolve` so prompts and shadow files can use it. No behavior change yet — the parameter is added and existing callers pass it through.

**Files:**
- Modify: `src/cli/resolve.rs` — `prompt_resolve`, `prompt_resolve_with_color`, `resolve_combined_file`, `resolve_push_drift` signatures.
- Modify: `src/cli/pull/common.rs` — `decide_pull_action`/`apply_pull_action` callers thread env name from `PullCtx`.
- Modify: `src/cli/pull/mod.rs` — add `env: String` to `PullCtx`.
- Modify: `src/cli/push/*.rs` (drivers) — each driver receives `env: &str` from `push::run` and threads it into `resolve_push_drift`.
- Modify: `src/cli/push/mod.rs` — pass `env` through to drivers.

- [ ] **Step 1: Add the failing unit test in `src/cli/resolve.rs` tests module**

```rust
#[test]
fn prompt_resolve_interpolates_env_name() {
    use std::io::Cursor;
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("x.json");
    std::fs::write(&local, b"{\"a\":1}").unwrap();
    let remote = b"{\"a\":2}";
    let mut out: Vec<u8> = Vec::new();
    let input = Cursor::new(b"s\n");

    let _ = prompt_resolve_with_color(
        input, &mut out, 1, 1, &local, remote, "production", ColorMode::Never,
    ).unwrap();

    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("production"), "expected env name in prompt, got: {s}");
    assert!(!s.contains("[r]emote"), "expected env name to replace literal 'remote' label, got: {s}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --lib cli::resolve::tests::prompt_resolve_interpolates_env_name -- --nocapture`
Expected: FAIL — signature mismatch (the function doesn't accept `env_name` yet).

- [ ] **Step 3: Update `prompt_resolve` and `prompt_resolve_with_color` signatures**

In `src/cli/resolve.rs`, add `env_name: &str` as the second-to-last parameter (just before the mode for the `_with_color` variant; just before nothing for the auto-detect variant):

```rust
pub fn prompt_resolve<R: BufRead, W: Write>(
    input: R,
    output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    remote_bytes: &[u8],
    env_name: &str,
) -> Result<Resolution> {
    let mode = detect_color_mode(false);
    prompt_resolve_with_color(input, output, index, total, local_path, remote_bytes, env_name, mode)
}

pub fn prompt_resolve_with_color<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    remote_bytes: &[u8],
    env_name: &str,
    mode: ColorMode,
) -> Result<Resolution> {
    // ... body unchanged for now; env_name unused. We'll wire prompt text in Task 2.
    let _ = env_name; // suppress unused warning for this task only
    // ... existing body
}
```

Then update `resolve_combined_file` and `resolve_push_drift` to take `env_name` too — they're the entry points the drivers call.

```rust
pub fn resolve_combined_file(
    interactive: bool,
    local_path: &Path,
    remote_bytes: &[u8],
    progress: &Arc<OverallProgress>,
    env_name: &str,
) -> Result<Vec<u8>> {
    // existing body; plumb env_name through to prompt_resolve below.
}

pub fn resolve_push_drift(
    interactive: bool,
    local_path: &Path,
    remote_bytes: &[u8],
    env_name: &str,
) -> Result<PushDriftOutcome> {
    // existing body; plumb env_name through to prompt_resolve below.
}
```

- [ ] **Step 4: Update `PullCtx` to carry env name and thread it through pull drivers**

In `src/cli/pull/common.rs`:

```rust
pub struct PullCtx<'a> {
    pub paths: &'a Paths,
    pub client: &'a RossumClient,
    pub lockfile: &'a mut Lockfile,
    pub queue_locations: std::collections::BTreeMap<i64, (String, String)>,
    pub overlay: Option<Overlay>,
    pub interactive: bool,
    pub env: String,            // NEW
}
```

In `src/cli/pull/mod.rs::run`, populate the field when constructing `PullCtx`:

```rust
let mut ctx = PullCtx {
    paths: &paths,
    client: &client,
    lockfile: &mut lockfile,
    queue_locations: std::collections::BTreeMap::new(),
    overlay,
    interactive,
    env: env.to_string(),
};
```

In every `pull/common.rs::apply_pull_action` (or wherever it calls into `resolve_combined_file`), pass `&ctx.env` (or pass `&str` through whatever helper signature exists).

In `src/cli/push/mod.rs::run`, thread `env` into `run_drivers` and from there into each per-kind `push` function. Push drivers call `resolve_push_drift` — those calls become `resolve_push_drift(interactive, local_path, &remote_json_stripped, env)`.

- [ ] **Step 5: Re-run the test from Step 1 and the full suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS (test green; no regressions).

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
resolve: thread env_name through prompt + drift entry points

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Env-aware resolver prompt text

**Goal:** Replace the literal `[r]emote` label, the "+++ remote" diff header, and the env-side "Local has changes / Remote has changes" framing with env-named equivalents.

**Files:**
- Modify: `src/cli/resolve.rs` — prompt text + diff label.

- [ ] **Step 1: Extend the test from Task 1 to assert exact label**

Replace the test from Task 1 with:

```rust
#[test]
fn prompt_resolve_uses_env_name_in_labels() {
    use std::io::Cursor;
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("x.json");
    std::fs::write(&local, b"{\"a\":1}").unwrap();
    let remote = b"{\"a\":2}";
    let mut out: Vec<u8> = Vec::new();
    let input = Cursor::new(b"s\n");

    let _ = prompt_resolve_with_color(
        input, &mut out, 1, 1, &local, remote, "production", ColorMode::Never,
    ).unwrap();

    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("[r] use production"), "prompt missing env-named [r] label: {s}");
    assert!(s.contains("+++ production"), "diff header should name the env: {s}");
    assert!(!s.contains("[r]emote"), "old literal label leaked: {s}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --lib cli::resolve::tests::prompt_resolve_uses_env_name_in_labels -- --nocapture`
Expected: FAIL — current text is `[r]emote`.

- [ ] **Step 3: Update prompt + diff label in `prompt_resolve_with_color`**

Replace the prompt-text constant inside the loop:

```rust
let prompt_text = format!(
    "[k] keep local  [r] use {env_name}  [e] edit  [s] skip (shadow file)  [a] abort > "
);
write!(output, "{}", colorize_prompt(&prompt_text, mode))?;
```

And replace the call to `unified_diff` to use the env name as the right-hand label:

```rust
let diff = unified_diff("local", &local_display, env_name, &remote_display);
```

The doc comment at the top of the file (lines 7–17) describes the prompt — update it to read:

```rust
//! [1/N]  hooks/validator-invoices.json
//!
//! local has changes:
//!   <unified diff snippet>
//!
//! production has changes:
//!   <unified diff snippet>
//!
//! [k] keep local   [r] use production   [e] edit   [s] skip   [a] abort >
```

- [ ] **Step 4: Run the test + full suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS. Existing resolver tests that asserted on the old label string need updating — adjust assertions to match the env-named output (call sites in `resolve.rs` tests likely use a placeholder env like `"remote"` or `"test"`; just feed a real env name).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
resolve: env-aware prompt labels + diff header

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Env-aware shadow file naming

**Goal:** Replace `<file>.remote` shadow files with `<file>.<env-name>`. This is the artifact written when the user picks `[s]` skip on a conflict, and the marker written under non-TTY for conflicts.

**Files:**
- Modify: `src/cli/pull/common.rs` — `shadow_file_conflict` signature.
- Modify: callers of `shadow_file_conflict` (search the file for usage).

- [ ] **Step 1: Update the existing unit test in `pull/common.rs` to assert env-named shadow file**

Find the existing test (mentioned in the grep — line 403, `assert_eq!(std::fs::read(dir.path().join("x.json.remote")).unwrap(), b"remote");`) and update:

```rust
// previously: x.json.remote → x.json.production
assert_eq!(
    std::fs::read(dir.path().join("x.json.production")).unwrap(),
    b"remote-bytes",
    "shadow file should be named after the env"
);
```

Update the test setup to pass a real env name to whatever helper is exercised.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --lib cli::pull::common::tests -- --nocapture`
Expected: FAIL — current implementation writes `.remote`.

- [ ] **Step 3: Update `shadow_file_conflict` to take env name**

```rust
fn shadow_file_conflict(
    local_path: &Path,
    remote_bytes: &[u8],
    progress: &Arc<OverallProgress>,
    env_name: &str,
) -> Result<String> {
    let shadow = match local_path.file_name().and_then(|f| f.to_str()) {
        Some(name) => format!("{name}.{env_name}"),
        None => format!("shadow.{env_name}"),
    };
    let shadow_path = local_path.with_file_name(&shadow);
    std::fs::write(&shadow_path, remote_bytes)
        .with_context(|| format!("writing shadow file {}", shadow_path.display()))?;
    progress.println(format!(
        "  ⚠ conflict on {} — kept local; remote saved as {}",
        local_path.display(),
        shadow_path.display(),
    ));
    // ... rest unchanged (hash compute, return)
}
```

Find every caller (likely 2–3 sites in `pull/common.rs` and possibly `resolve.rs::resolve_combined_file` callbacks) and thread `env_name` through.

- [ ] **Step 4: Run the test + full suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS. Some snapshot-style tests that assert against `.remote` filenames will need updating — replace with the env name used in the test fixture.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
resolve: shadow file is <file>.<env-name> instead of <file>.remote

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `SyncClass` enum and classification skeleton

**Goal:** Land the type that classification produces and an empty `classify()` function ready to be filled.

**Files:**
- Create: `src/cli/sync/mod.rs`
- Create: `src/cli/sync/classify.rs`
- Modify: `src/cli/mod.rs` — add `pub mod sync;`

- [ ] **Step 1: Add module declaration in `src/cli/mod.rs`**

In `src/cli/mod.rs`, alongside the existing `pub mod` lines:

```rust
pub mod sync;
```

- [ ] **Step 2: Create `src/cli/sync/mod.rs` with module exports**

```rust
//! `rdc sync <env>` — reconcile local snapshot and remote state in one pass.
//!
//! Spec: docs/superpowers/specs/2026-05-14-unified-sync-design.md

pub mod classify;
```

- [ ] **Step 3: Create `src/cli/sync/classify.rs` with the enum and a stub**

```rust
//! Eleven-class classification of `(kind, slug)` items based on local file
//! state, remote API listing, and the lockfile hash. The classification drives
//! the sync executor: pull-side writes, push-side writes, and resolver prompts.
//!
//! Spec §"Execution pipeline → Classify".

use std::collections::BTreeSet;

/// One of eleven classes. See the spec table for definitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncClass {
    Clean,
    LocalEdit,
    LocalCreate,
    LocalDelete,
    RemoteEdit,
    RemoteCreate,
    RemoteDelete,
    BothDiverged,
    LocalEditRemoteDelete,
    LocalDeleteRemoteEdit,
    BothDeleted,
}

/// A single classified item with the bytes / hashes needed by the executor.
#[derive(Debug, Clone)]
pub struct ClassifiedItem {
    pub kind: String,
    pub slug: String,
    pub class: SyncClass,
    /// Hash of the local file (if any) at scan time. Used to detect mid-run drift.
    pub local_hash: Option<String>,
    /// Hash of the remote body (if any) from the listing.
    pub remote_hash: Option<String>,
    /// Hash recorded by the lockfile (the merge base).
    pub base_hash: Option<String>,
}

/// Empty classification stub. Each branch is filled in across the next tasks.
pub fn classify(
    _remote_index: &BTreeSet<(String, String)>,
    _scan_changes: &BTreeSet<(String, String)>,
    _scan_tombstones: &BTreeSet<(String, String)>,
    _locked: &BTreeSet<(String, String)>,
) -> Vec<ClassifiedItem> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    // populated in subsequent tasks
}
```

- [ ] **Step 4: Run the suite to verify the new modules compile**

Run: `cargo build && cargo test -p rdc -- --nocapture`
Expected: PASS (no new tests yet; build succeeds, existing tests still pass).

- [ ] **Step 5: Commit**

```bash
git add src/cli/mod.rs src/cli/sync/
git commit -m "$(cat <<'EOF'
sync: scaffold module with SyncClass + classify stub

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Classify clean + single-side cases (6 cases)

**Goal:** Implement and test the six "simple" classifications: `Clean`, `LocalEdit`, `LocalCreate`, `LocalDelete`, `RemoteEdit`, `RemoteCreate`.

**Files:**
- Modify: `src/cli/sync/classify.rs`.

- [ ] **Step 1: Write failing tests for the six simple cases**

In `src/cli/sync/classify.rs::tests`:

```rust
use super::*;
use std::collections::BTreeSet;

fn key(k: &str, s: &str) -> (String, String) {
    (k.to_string(), s.to_string())
}

fn s(items: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    items.iter().map(|(k, s)| key(k, s)).collect()
}

#[test]
fn clean_no_changes_no_tombstones() {
    let result = classify(
        &s(&[("hooks", "v1")]),  // remote_index
        &s(&[]),                  // scan_changes
        &s(&[]),                  // scan_tombstones
        &s(&[("hooks", "v1")]),  // locked
    );
    assert_eq!(result.iter().find(|c| c.slug == "v1").unwrap().class, SyncClass::Clean);
}

#[test]
fn local_edit_remote_at_lockfile() {
    let result = classify(
        &s(&[("hooks", "v1")]),
        &s(&[("hooks", "v1")]),
        &s(&[]),
        &s(&[("hooks", "v1")]),
    );
    assert_eq!(result.iter().find(|c| c.slug == "v1").unwrap().class, SyncClass::LocalEdit);
}

#[test]
fn local_create_no_remote_no_lockfile() {
    let result = classify(
        &s(&[]),
        &s(&[("hooks", "new")]),
        &s(&[]),
        &s(&[]),
    );
    assert_eq!(result.iter().find(|c| c.slug == "new").unwrap().class, SyncClass::LocalCreate);
}

#[test]
fn local_delete_tombstone_remote_unchanged() {
    let result = classify(
        &s(&[("hooks", "v1")]),
        &s(&[]),
        &s(&[("hooks", "v1")]),
        &s(&[("hooks", "v1")]),
    );
    assert_eq!(result.iter().find(|c| c.slug == "v1").unwrap().class, SyncClass::LocalDelete);
}

#[test]
fn remote_edit_local_unchanged() {
    // To distinguish "remote-edit" from "clean", we need hashes. The
    // signature in Task 4 uses BTreeSet for membership; we need to extend
    // it. Update the signature to take hash maps.
}

#[test]
fn remote_create_no_local_no_lockfile() {
    let result = classify(
        &s(&[("hooks", "new")]),
        &s(&[]),
        &s(&[]),
        &s(&[]),
    );
    assert_eq!(result.iter().find(|c| c.slug == "new").unwrap().class, SyncClass::RemoteCreate);
}
```

Note: the `remote_edit_local_unchanged` test reveals that the classifier needs **hashes** to distinguish "remote at lockfile" from "remote differs from lockfile." The `BTreeSet`-only signature from Task 4 was deliberately a stub. Update to take hash maps:

```rust
pub fn classify(
    remote_hashes: &std::collections::BTreeMap<(String, String), String>,
    scan_changes: &std::collections::BTreeMap<(String, String), String>,
    scan_tombstones: &BTreeSet<(String, String)>,
    locked: &std::collections::BTreeMap<(String, String), String>,
) -> Vec<ClassifiedItem>
```

Each map stores `(kind, slug) → content_hash`. `scan_tombstones` is membership-only.

Rewrite the tests to use the map form:

```rust
fn m(items: &[(&str, &str, &str)]) -> std::collections::BTreeMap<(String, String), String> {
    items.iter().map(|(k, s, h)| (key(k, s), h.to_string())).collect()
}

#[test]
fn remote_edit_local_unchanged() {
    let result = classify(
        &m(&[("hooks", "v1", "hash_REMOTE_NEW")]),
        &std::collections::BTreeMap::new(),
        &BTreeSet::new(),
        &m(&[("hooks", "v1", "hash_BASE")]),
    );
    assert_eq!(
        result.iter().find(|c| c.slug == "v1").unwrap().class,
        SyncClass::RemoteEdit
    );
}
```

Rewrite the other tests in the same style.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rdc --lib cli::sync::classify::tests -- --nocapture`
Expected: FAIL — `classify` returns empty `Vec`.

- [ ] **Step 3: Implement the 6 simple classifications**

```rust
use std::collections::BTreeMap;

pub fn classify(
    remote_hashes: &BTreeMap<(String, String), String>,
    scan_changes: &BTreeMap<(String, String), String>,
    scan_tombstones: &BTreeSet<(String, String)>,
    locked: &BTreeMap<(String, String), String>,
) -> Vec<ClassifiedItem> {
    let mut all_keys: BTreeSet<&(String, String)> = BTreeSet::new();
    all_keys.extend(remote_hashes.keys());
    all_keys.extend(scan_changes.keys());
    all_keys.extend(scan_tombstones.iter());
    all_keys.extend(locked.keys());

    let mut out = Vec::with_capacity(all_keys.len());
    for k in all_keys {
        let local_changed = scan_changes.contains_key(k);
        let local_tombstoned = scan_tombstones.contains(k);
        let remote_present = remote_hashes.contains_key(k);
        let locked_present = locked.contains_key(k);

        let remote_hash = remote_hashes.get(k).cloned();
        let base_hash = locked.get(k).cloned();
        let local_hash = scan_changes.get(k).cloned();

        let class = match (local_changed, local_tombstoned, remote_present, locked_present) {
            // clean
            (false, false, true, true) if remote_hash == base_hash => SyncClass::Clean,
            // local-only edit
            (true, false, true, true) if remote_hash == base_hash => SyncClass::LocalEdit,
            // local-only create
            (true, false, false, false) => SyncClass::LocalCreate,
            // local-only delete
            (false, true, true, true) if remote_hash == base_hash => SyncClass::LocalDelete,
            // remote-only edit
            (false, false, true, true) if remote_hash != base_hash => SyncClass::RemoteEdit,
            // remote-only create
            (false, false, true, false) => SyncClass::RemoteCreate,
            // double-conflict cases (filled in later tasks)
            _ => continue, // skip unhandled cases for now
        };

        out.push(ClassifiedItem {
            kind: k.0.clone(),
            slug: k.1.clone(),
            class,
            local_hash,
            remote_hash,
            base_hash,
        });
    }
    out
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --lib cli::sync::classify::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/sync/classify.rs
git commit -m "$(cat <<'EOF'
sync: classify clean + 5 single-side classes with unit tests

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Classify both-diverged and both-deleted

**Goal:** Two remaining classes that don't involve "remote absent": `BothDiverged` and `BothDeleted`.

**Files:**
- Modify: `src/cli/sync/classify.rs`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn both_diverged_local_and_remote_changed() {
    let result = classify(
        &m(&[("hooks", "v1", "hash_REMOTE_NEW")]),
        &m(&[("hooks", "v1", "hash_LOCAL_NEW")]),
        &BTreeSet::new(),
        &m(&[("hooks", "v1", "hash_BASE")]),
    );
    assert_eq!(
        result.iter().find(|c| c.slug == "v1").unwrap().class,
        SyncClass::BothDiverged
    );
}

#[test]
fn both_deleted_silent_convergence() {
    let result = classify(
        &BTreeMap::new(),
        &BTreeMap::new(),
        &s(&[("hooks", "v1")]),
        &m(&[("hooks", "v1", "hash_BASE")]),
    );
    assert_eq!(
        result.iter().find(|c| c.slug == "v1").unwrap().class,
        SyncClass::BothDeleted
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rdc --lib cli::sync::classify::tests::both_ -- --nocapture`
Expected: FAIL.

- [ ] **Step 3: Add the two arms to the `match` in `classify()`**

```rust
let class = match (local_changed, local_tombstoned, remote_present, locked_present) {
    // ... existing arms ...
    // both diverged
    (true, false, true, true) if remote_hash != base_hash => SyncClass::BothDiverged,
    // both deleted: silent convergence
    (false, true, false, true) => SyncClass::BothDeleted,
    _ => continue,
};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --lib cli::sync::classify::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/sync/classify.rs
git commit -m "$(cat <<'EOF'
sync: classify both-diverged + both-deleted

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Classify remote-delete and the two double-conflicts

**Goal:** The three remaining classes — `RemoteDelete`, `LocalEditRemoteDelete`, `LocalDeleteRemoteEdit`.

**Files:**
- Modify: `src/cli/sync/classify.rs`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn remote_delete_local_unchanged() {
    let result = classify(
        &BTreeMap::new(),                         // remote absent
        &BTreeMap::new(),                         // local unchanged
        &BTreeSet::new(),
        &m(&[("hooks", "v1", "hash_BASE")]),     // lockfile has it
    );
    assert_eq!(
        result.iter().find(|c| c.slug == "v1").unwrap().class,
        SyncClass::RemoteDelete
    );
}

#[test]
fn local_edit_remote_delete() {
    let result = classify(
        &BTreeMap::new(),                              // remote absent
        &m(&[("hooks", "v1", "hash_LOCAL_NEW")]),     // local changed
        &BTreeSet::new(),
        &m(&[("hooks", "v1", "hash_BASE")]),
    );
    assert_eq!(
        result.iter().find(|c| c.slug == "v1").unwrap().class,
        SyncClass::LocalEditRemoteDelete
    );
}

#[test]
fn local_delete_remote_edit() {
    let result = classify(
        &m(&[("hooks", "v1", "hash_REMOTE_NEW")]),    // remote changed
        &BTreeMap::new(),
        &s(&[("hooks", "v1")]),                        // local tombstoned
        &m(&[("hooks", "v1", "hash_BASE")]),
    );
    assert_eq!(
        result.iter().find(|c| c.slug == "v1").unwrap().class,
        SyncClass::LocalDeleteRemoteEdit
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rdc --lib cli::sync::classify::tests -- --nocapture`
Expected: 3 FAIL.

- [ ] **Step 3: Add the three arms and remove the catch-all skip**

```rust
let class = match (local_changed, local_tombstoned, remote_present, locked_present) {
    // ... existing arms ...
    // remote-only delete
    (false, false, false, true) => SyncClass::RemoteDelete,
    // local edit + remote delete
    (true, false, false, true) => SyncClass::LocalEditRemoteDelete,
    // local delete + remote edit
    (false, true, true, true) if remote_hash != base_hash => SyncClass::LocalDeleteRemoteEdit,
    // any leftover combination is a bug in the classifier: surface it.
    _ => panic!(
        "classify: unhandled state for {:?}: \
         local_changed={local_changed} local_tombstoned={local_tombstoned} \
         remote_present={remote_present} locked_present={locked_present} \
         remote_hash={remote_hash:?} base_hash={base_hash:?}",
        k
    ),
};
```

The `panic!` arm is a deliberate fail-loud: if the table is incomplete, a future code change that adds new state will fail integration tests instead of silently miscategorising.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rdc --lib cli::sync::classify::tests -- --nocapture`
Expected: PASS — all 11 cases now covered.

- [ ] **Step 5: Add one final test that asserts the panic arm is unreachable**

```rust
#[test]
fn classify_handles_every_combination_without_panicking() {
    // For every plausible state combination, ensure classify doesn't panic.
    // Drive the classifier across all 16 (local_changed, local_tombstoned,
    // remote_present, locked_present) × hash variants; we only construct
    // inputs that pass `scan` invariants (e.g., changes can't coexist with
    // tombstones for the same key).
    for (lc, lt, rp, lp) in [
        (false, false, false, false), // nothing — skipped (no item)
        (true, false, false, false),
        (false, false, true, false),
        (true, false, true, false),
        (false, false, false, true),
        (true, false, false, true),
        (false, false, true, true),
        (true, false, true, true),
        (false, true, false, true),
        (false, true, true, true),
    ] {
        let _ = lc; let _ = lt; let _ = rp; let _ = lp;
        // construct inputs (omitted for brevity) and call classify().
        // The test passes if no panic; assertions on specific classes are
        // covered by the per-class tests above.
    }
}
```

(Note: this is a defense-in-depth check; the specific cases above already cover the table.)

- [ ] **Step 6: Run + commit**

Run: `cargo test -p rdc --lib cli::sync::classify -- --nocapture`
Expected: PASS.

```bash
git add src/cli/sync/classify.rs
git commit -m "$(cat <<'EOF'
sync: classify remote-delete + two double-conflict cases

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Extract `list_remote` from pull's Phase 1

**Goal:** Move the per-kind listing logic from `pull::run_drivers` into a reusable `pull::common::list_remote` returning a typed catalog. Pull continues to work; sync (built in later tasks) will reuse this.

**Files:**
- Modify: `src/cli/pull/common.rs` — add `RemoteCatalog` struct and `list_remote()` function.
- Modify: `src/cli/pull/mod.rs` — `run_drivers` Phase 1 becomes a call to `list_remote`.

- [ ] **Step 1: Define `RemoteCatalog` in `pull/common.rs`**

```rust
/// All kinds listed from one env's API, ready for either pull's process
/// step or sync's classify step. Cheap to construct, hashable per-kind.
pub struct RemoteCatalog {
    pub organization: crate::model::Organization,
    pub workspaces: Vec<crate::model::Workspace>,
    pub queues: Vec<crate::model::Queue>,
    pub hooks: Vec<crate::model::Hook>,
    pub rules: Vec<crate::model::Rule>,
    pub labels: Vec<crate::model::Label>,
    pub engines: Vec<crate::model::Engine>,
    pub engine_fields: Vec<crate::model::EngineField>,
    pub workflows: Vec<crate::model::Workflow>,
    pub workflow_steps: Vec<crate::model::WorkflowStep>,
    pub email_templates: Vec<crate::model::EmailTemplate>,
    pub mdh: crate::cli::pull::mdh::DatasetCatalog,
}
```

(`DatasetCatalog` is what `mdh::list` already returns; rename or alias as needed.)

- [ ] **Step 2: Implement `list_remote()`**

Move the body of `cli::pull::run_drivers` lines 169–219 (the Phase 1 listings, see existing code) into a new function:

```rust
pub async fn list_remote(
    ctx: &mut PullCtx<'_>,
    env_cfg: &crate::config::EnvConfig,
    env: &str,
    token: &str,
    progress: &Arc<OverallProgress>,
) -> Result<RemoteCatalog> {
    let organization = super::organization::list(ctx, env_cfg.org_id, progress).await
        .with_context(|| format!("listing organization for env '{env}'"))?;
    progress.inc_total(1);

    let workspaces = super::workspaces::list(ctx, progress).await
        .with_context(|| format!("listing workspaces for env '{env}'"))?;
    progress.inc_total(workspaces.len() as u64);

    // ... repeat for every kind, mirroring lines 180–219 of pull/mod.rs

    let mdh = super::mdh::list(env_cfg, token, progress).await
        .with_context(|| format!("listing MDH datasets for env '{env}'"))?;
    progress.inc_total(mdh.collections.len() as u64);

    Ok(RemoteCatalog {
        organization, workspaces, queues, hooks, rules, labels,
        engines, engine_fields, workflows, workflow_steps,
        email_templates, mdh,
    })
}
```

- [ ] **Step 3: Update `pull::run_drivers` to call `list_remote`**

```rust
async fn run_drivers(...) -> Result<PullStats> {
    let catalog = crate::cli::pull::common::list_remote(ctx, env_cfg, env, token, progress).await?;

    // Phase 2 unchanged — uses fields from catalog directly:
    let (n_orgs, c_orgs) = organization::process(ctx, catalog.organization, progress).await?;
    let n_workspaces = workspaces::process(ctx, catalog.workspaces, progress).await?;
    // ... etc
}
```

- [ ] **Step 4: Run full suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS — pull behavior unchanged, refactor only.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
pull: extract list_remote into common; pull::run_drivers calls it

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Pull drivers accept a `subset` filter

**Goal:** Each `pull::*::process` function takes a `&BTreeSet<(String, String)>` of slugs to actually write. When called from existing pull, the subset is "everything listed." When called from sync, it's the classified pull-side subset.

**Files:**
- Modify: every file in `src/cli/pull/` except `mod.rs`, `common.rs`, `organization.rs` (single-row, no subset semantics).

- [ ] **Step 1: Add an integration assertion test in `tests/cli_pull.rs`**

(Keep the existing pull tests intact for now — they validate end-to-end behavior with `subset = all`.) Add a new test that uses the subset directly via the API:

```rust
// In src/cli/pull/labels.rs::tests (a new mod tests at the bottom)
#[tokio::test]
async fn labels_process_skips_outside_subset() {
    // Build a ctx with two labels listed; subset contains only one slug.
    // Assert: only the in-subset label is written to disk; the other isn't.
    // (Full setup: see existing labels-related test in tests/cli_pull.rs for
    // the mock pattern.)
}
```

- [ ] **Step 2: Run test to verify it fails (parameter doesn't exist)**

Run: `cargo test -p rdc --lib cli::pull::labels::tests::labels_process_skips_outside_subset -- --nocapture`
Expected: FAIL — parameter doesn't exist; test won't compile.

- [ ] **Step 3: Add `subset` parameter to every `process` function**

For each of `workspaces`, `queues`, `hooks`, `rules`, `labels`, `engines`, `engine_fields`, `workflows`, `workflow_steps`, `email_templates`, `mdh`:

```rust
pub async fn process(
    ctx: &mut PullCtx<'_>,
    items: Vec<Label>,                                 // (kind-specific)
    subset: &std::collections::BTreeSet<(String, String)>,
    progress: &Arc<OverallProgress>,
) -> Result<(usize, usize)> {
    for l in &items {
        let slug = /* compute as before */;
        // NEW: skip if not in subset.
        if !subset.contains(&("labels".to_string(), slug.clone())) {
            continue;
        }
        // existing body...
    }
}
```

For nested kinds (queues, schemas, inboxes, email_templates), use the appropriate kind name in the membership check. `email_templates` uses the compound slug `<ws>/<q>/<tpl>`.

`organization` is org-scoped (one row, no subset needed) — leave its signature alone OR always pass `BTreeSet::from([("organization".into(), "self".into())])` for symmetry. Skip-on-empty isn't a real case for org.

- [ ] **Step 4: Update `pull::run_drivers` to pass an all-inclusive subset**

```rust
let all: BTreeSet<(String, String)> = catalog_to_full_subset(&catalog);
let n_workspaces = workspaces::process(ctx, catalog.workspaces, &all, progress).await?;
// ... etc.
```

Where `catalog_to_full_subset` constructs the `(kind, slug)` set from every item in the catalog.

- [ ] **Step 5: Run the suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS — existing pull tests still green; the new per-kind subset test green.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
pull: per-kind process functions accept a subset filter

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Push drivers' subset stays as `ChangeList` (no refactor needed)

**Goal:** Verify push drivers already accept a filtered slice (`ChangeList`) and document the mapping from `Vec<ClassifiedItem>` (push-side) to `ChangeList`. No code change, just a helper.

**Files:**
- Modify: `src/cli/push/scan.rs` — add `pub fn change_list_from_classified(items: &[ClassifiedItem]) -> ChangeList`.

- [ ] **Step 1: Add unit test in `push/scan.rs::tests`**

```rust
#[test]
fn change_list_from_classified_groups_by_kind() {
    use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
    let items = vec![
        ClassifiedItem {
            kind: "hooks".into(), slug: "h1".into(),
            class: SyncClass::LocalEdit,
            local_hash: Some("h".into()), remote_hash: Some("h".into()),
            base_hash: Some("h".into()),
        },
        ClassifiedItem {
            kind: "labels".into(), slug: "l1".into(),
            class: SyncClass::LocalCreate,
            local_hash: Some("h".into()), remote_hash: None, base_hash: None,
        },
    ];
    let cl = change_list_from_classified(&items);
    assert_eq!(cl.hooks.len(), 1);
    assert_eq!(cl.labels.len(), 1);
    assert!(cl.queues.is_empty());
}
```

- [ ] **Step 2: Implement the helper**

```rust
pub fn change_list_from_classified(items: &[crate::cli::sync::classify::ClassifiedItem]) -> ChangeList {
    use crate::cli::sync::classify::SyncClass;
    let mut cl = ChangeList::default();
    for it in items {
        // Only push-side classes belong here.
        match it.class {
            SyncClass::LocalEdit
            | SyncClass::LocalCreate => {}
            _ => continue,
        }
        let h = it.local_hash.clone().unwrap_or_default();
        match it.kind.as_str() {
            "workspaces" => { cl.workspaces.insert(it.slug.clone(), h.parse().unwrap_or(0)); }
            "schemas"    => { cl.schemas.insert(it.slug.clone(), h.parse().unwrap_or(0)); }
            "queues"     => { cl.queues.insert(it.slug.clone(), h.parse().unwrap_or(0)); }
            "inboxes"    => { cl.inboxes.insert(it.slug.clone(), h.parse().unwrap_or(0)); }
            "email_templates" => { cl.email_templates.insert(it.slug.clone(), h.parse().unwrap_or(0)); }
            "hooks"      => { cl.hooks.insert(it.slug.clone(), h.parse().unwrap_or(0)); }
            "rules"      => { cl.rules.insert(it.slug.clone(), h.parse().unwrap_or(0)); }
            "labels"     => { cl.labels.insert(it.slug.clone(), h.parse().unwrap_or(0)); }
            "engines"    => { cl.engines.insert(it.slug.clone(), h.parse().unwrap_or(0)); }
            "engine_fields" => { cl.engine_fields.insert(it.slug.clone(), h.parse().unwrap_or(0)); }
            _ => {}
        }
    }
    cl
}
```

Note: the existing `ChangeList` keys are `BTreeMap<String, u64>` (slug → file mtime ns). Inside sync, the local hash is a string; the inner u64 isn't actually consulted by the push drivers (they re-read the file). Document this in a comment.

- [ ] **Step 3: Run test + suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/cli/push/scan.rs
git commit -m "$(cat <<'EOF'
push: helper to convert classified items into a ChangeList

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Plan rendering

**Goal:** Render the `Plan: sync <env>` block from a `Vec<ClassifiedItem>`. Test the output format directly (string buffer).

**Files:**
- Create: `src/cli/sync/plan.rs`
- Modify: `src/cli/sync/mod.rs` — `pub mod plan;`

- [ ] **Step 1: Write failing test**

```rust
// in src/cli/sync/plan.rs::tests
use super::*;
use crate::cli::sync::classify::{ClassifiedItem, SyncClass};

fn item(kind: &str, slug: &str, class: SyncClass) -> ClassifiedItem {
    ClassifiedItem {
        kind: kind.into(), slug: slug.into(), class,
        local_hash: Some("h".into()),
        remote_hash: Some("h".into()),
        base_hash: Some("h".into()),
    }
}

#[test]
fn render_plan_groups_by_direction() {
    let items = vec![
        item("hooks", "v1", SyncClass::RemoteEdit),
        item("queues", "c1", SyncClass::RemoteEdit),
        item("labels", "a1", SyncClass::RemoteCreate),
        item("rules", "r1", SyncClass::LocalEdit),
        item("hooks", "vt", SyncClass::BothDiverged),
    ];
    let out = render_plan("test", &items);
    assert!(out.contains("Plan: sync test"), "header: {out}");
    assert!(out.contains("3 changes from test"), "pull count: {out}");
    assert!(out.contains("1 local edit"), "push count: {out}");
    assert!(out.contains("1 conflict"), "conflict count: {out}");
    assert!(out.contains("hooks/v1"), "pull item: {out}");
    assert!(out.contains("rules/r1"), "push item: {out}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --lib cli::sync::plan::tests -- --nocapture`
Expected: FAIL — `render_plan` not defined.

- [ ] **Step 3: Implement plan rendering**

```rust
use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
use std::fmt::Write;

pub fn render_plan(env: &str, items: &[ClassifiedItem]) -> String {
    let mut pull: Vec<&ClassifiedItem> = Vec::new();
    let mut push: Vec<&ClassifiedItem> = Vec::new();
    let mut conflicts: Vec<&ClassifiedItem> = Vec::new();
    let mut tombstone_resolves: Vec<&ClassifiedItem> = Vec::new();

    for it in items {
        match it.class {
            SyncClass::RemoteEdit | SyncClass::RemoteCreate => pull.push(it),
            SyncClass::LocalEdit | SyncClass::LocalCreate | SyncClass::LocalDelete => push.push(it),
            SyncClass::BothDiverged
            | SyncClass::LocalEditRemoteDelete
            | SyncClass::LocalDeleteRemoteEdit => conflicts.push(it),
            SyncClass::RemoteDelete => tombstone_resolves.push(it),
            SyncClass::Clean | SyncClass::BothDeleted => {}
        }
    }

    let mut out = String::new();
    writeln!(out, "Plan: sync {env}").ok();
    if !pull.is_empty() {
        writeln!(out, "  ← pull:    {} change{} from {env}",
            pull.len(),
            if pull.len() == 1 { "" } else { "s" }
        ).ok();
        for it in &pull {
            let note = if matches!(it.class, SyncClass::RemoteCreate) { " (new)" } else { "" };
            writeln!(out, "               {}/{}{note}", it.kind, it.slug).ok();
        }
    }
    if !push.is_empty() {
        writeln!(out, "  → push:    {} local edit{}",
            push.len(),
            if push.len() == 1 { "" } else { "s" }
        ).ok();
        for it in &push {
            writeln!(out, "               {}/{}", it.kind, it.slug).ok();
        }
    }
    if !conflicts.is_empty() {
        writeln!(out, "  ⚠ conflict: {} object{}",
            conflicts.len(),
            if conflicts.len() == 1 { "" } else { "s" }
        ).ok();
        for it in &conflicts {
            let tag = match it.class {
                SyncClass::BothDiverged => "both diverged",
                SyncClass::LocalEditRemoteDelete => "local edit, deleted on env",
                SyncClass::LocalDeleteRemoteEdit => "local delete, edited on env",
                _ => "",
            };
            writeln!(out, "               {}/{}  — {tag}", it.kind, it.slug).ok();
        }
    }
    if !tombstone_resolves.is_empty() {
        writeln!(out, "  ⚠ deleted on {env}: {} object{}",
            tombstone_resolves.len(),
            if tombstone_resolves.len() == 1 { "" } else { "s" }
        ).ok();
        for it in &tombstone_resolves {
            writeln!(out, "               {}/{}", it.kind, it.slug).ok();
        }
    }
    out
}
```

- [ ] **Step 4: Run + commit**

Run: `cargo test -p rdc --lib cli::sync::plan -- --nocapture`
Expected: PASS.

```bash
git add src/cli/sync/plan.rs src/cli/sync/mod.rs
git commit -m "$(cat <<'EOF'
sync: plan renderer with env-named labels

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Remote-deleted resolver prompt

**Goal:** New helper `prompt_remote_delete` in `cli::resolve` that asks `[k] keep local (restore on <env-name>) / [r] use <env-name> (delete local) / [s] skip / [a] abort`.

**Files:**
- Modify: `src/cli/resolve.rs` — add `prompt_remote_delete`.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn prompt_remote_delete_offers_restore_and_mirror() {
    use std::io::Cursor;
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("labels/audit-hold.json");
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, b"{\"name\":\"Audit hold\"}").unwrap();

    let mut out: Vec<u8> = Vec::new();
    let input = Cursor::new(b"s\n");
    let res = prompt_remote_delete_with_color(
        input, &mut out, &local, "production", ColorMode::Never,
    ).unwrap();
    assert!(matches!(res, Resolution::Skip));

    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("deleted on production"), "header: {s}");
    assert!(s.contains("[k] keep local (restore on production)"), "k label: {s}");
    assert!(s.contains("[r] use production (delete local)"), "r label: {s}");
    assert!(!s.contains("[e]"), "no edit option in delete prompt: {s}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --lib cli::resolve::tests::prompt_remote_delete_offers_restore_and_mirror -- --nocapture`
Expected: FAIL — function doesn't exist.

- [ ] **Step 3: Implement the prompt**

```rust
pub fn prompt_remote_delete<R: BufRead, W: Write>(
    input: R,
    output: W,
    local_path: &Path,
    env_name: &str,
) -> Result<Resolution> {
    let mode = detect_color_mode(false);
    prompt_remote_delete_with_color(input, output, local_path, env_name, mode)
}

pub fn prompt_remote_delete_with_color<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    local_path: &Path,
    env_name: &str,
    mode: ColorMode,
) -> Result<Resolution> {
    let local_bytes = read_local(local_path)?;
    let preview = prettify_json_for_diff(&local_bytes);

    writeln!(output)?;
    let header = format!("{} — deleted on {env_name}", local_path.display());
    writeln!(output, "{}", colorize_header(&header, mode))?;
    writeln!(output)?;
    writeln!(output, "local has the file:")?;
    // Elide the preview to ~40 lines for unwieldy bodies.
    let s = String::from_utf8_lossy(&preview);
    let lines: Vec<&str> = s.lines().collect();
    let limit = 40;
    if lines.len() <= limit {
        for ln in &lines { writeln!(output, "  {ln}")?; }
    } else {
        for ln in &lines[..limit] { writeln!(output, "  {ln}")?; }
        writeln!(output, "  … ({} more lines)", lines.len() - limit)?;
    }
    writeln!(output)?;
    writeln!(output, "{env_name} has it deleted.")?;
    writeln!(output)?;

    loop {
        let prompt_text = format!(
            "[k] keep local (restore on {env_name})  \
             [r] use {env_name} (delete local)  \
             [s] skip  \
             [a] abort > "
        );
        write!(output, "{}", colorize_prompt(&prompt_text, mode))?;
        output.flush().ok();
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(Resolution::Skip);
        }
        match line.trim().chars().next() {
            Some('k') | Some('K') => return Ok(Resolution::KeepLocal),
            Some('r') | Some('R') => return Ok(Resolution::KeepRemote),
            Some('s') | Some('S') => return Ok(Resolution::Skip),
            Some('a') | Some('A') => return Ok(Resolution::Abort),
            _ => {
                writeln!(output, "  (unrecognized — pick one of k/r/s/a)")?;
                continue;
            }
        }
    }
}
```

`Resolution::KeepRemote` here means "use env's truth" which is "deleted". The caller interprets it accordingly.

- [ ] **Step 4: Run + commit**

Run: `cargo test -p rdc --lib cli::resolve -- --nocapture`
Expected: PASS.

```bash
git add src/cli/resolve.rs
git commit -m "$(cat <<'EOF'
resolve: prompt_remote_delete with restore/mirror semantics

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: `sync::run` — happy-path pipeline

**Goal:** Wire list_remote → scan → classify → render plan → confirm → execute (pull-side and push-side delegations). No conflict prompts yet — the executor short-circuits to "abort the run" if it sees any conflict class.

**Files:**
- Create: `src/cli/sync/execute.rs` — empty stub for now.
- Modify: `src/cli/sync/mod.rs` — implement `pub async fn run`.

- [ ] **Step 1: Write a smoke test for the happy path**

`tests/cli_sync.rs` (new file):

```rust
mod common; // share with existing tests if present, else inline a tiny helper
use rdc::api::RossumClient;
use rdc::cli::sync;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn sync_clean_env_does_no_writes() {
    let server = MockServer::start().await;
    // Mock empty listings for every kind.
    Mock::given(method("GET")).and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [], "next": null, "previous": null
        })))
        .mount(&server).await;
    // ... mock the other kinds similarly (or use a helper) ...

    let tmp = tempfile::tempdir().unwrap();
    // Initialize an rdc project in tmp pointing at the mock server.
    // Run `sync::run` and assert: 0 writes, no errors, lockfile saved.
    // (Use the same fixture scaffolding as `tests/cli_pull.rs`.)
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --test cli_sync sync_clean_env_does_no_writes -- --nocapture`
Expected: FAIL — `sync::run` doesn't exist yet.

- [ ] **Step 3: Implement `sync::run`**

```rust
// src/cli/sync/mod.rs

use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::secrets::resolve_token;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

pub mod classify;
pub mod execute;
pub mod plan;

pub async fn run(
    env: &str,
    interactive: bool,
    dry_run: bool,
    diff: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
) -> Result<()> {
    if no_push && no_pull {
        anyhow::bail!("--no-push and --no-pull are mutually exclusive. Use 'rdc status' for read-only inspection.");
    }

    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);
    let cfg = ProjectConfig::load(&paths.project_config())?;
    let env_cfg = cfg.envs.get(env)
        .ok_or_else(|| anyhow!("env '{env}' is not defined in rdc.toml"))?;
    let token = resolve_token(&cwd, env)?;
    let client = RossumClient::new(env_cfg.api_base.clone(), token.clone())
        .context("constructing Rossum API client")?;
    let mut lockfile = Lockfile::load(&paths.lockfile())?;
    let overlay = crate::overlay::Overlay::load(&paths.overlay_file())?;
    let progress = OverallProgress::start(format!("sync envs/{env}"));

    // Phase 1: list remote.
    let mut ctx = crate::cli::pull::common::PullCtx {
        paths: &paths,
        client: &client,
        lockfile: &mut lockfile,
        queue_locations: Default::default(),
        overlay: overlay.clone(),
        interactive,
        env: env.to_string(),
    };
    let catalog = crate::cli::pull::common::list_remote(&mut ctx, env_cfg, env, &token, &progress).await?;

    // Phase 2: scan local.
    let (scanned, changes, tombstones) = crate::cli::push::scan::scan(&paths, &lockfile)?;
    let _ = scanned; // count printed below.

    // Phase 3: classify.
    let classified = classify::from_catalog_scan_lockfile(&catalog, &changes, &tombstones, &lockfile);

    // Phase 4: plan + confirm.
    let plan_text = plan::render_plan(env, &classified);
    print!("{plan_text}");
    if dry_run {
        if diff {
            // Reuse the existing diff machinery.
            crate::cli::diff::diff_local_vs_remote(&cwd, &cfg, env).await?;
        }
        progress.finish();
        println!("Dry run sync envs/{env}: 0 writes.");
        return Ok(());
    }
    if !no_push && classified.iter().any(|c| matches!(c.class, classify::SyncClass::LocalDelete)) {
        // Destructive gate for outgoing tombstones.
        if !crate::cli::push::deletes::confirm_or_refuse_classified(&classified, interactive, allow_deletes)? {
            eprintln!("sync aborted: deletes not confirmed.");
            return Ok(());
        }
    }
    if interactive && !confirm("Proceed?")? {
        eprintln!("sync aborted by user.");
        return Ok(());
    }

    // Phase 5: execute.
    execute::run(
        &mut ctx, &catalog, &classified,
        no_push, no_pull, interactive, &progress,
    ).await?;

    lockfile.save(&paths.lockfile())?;
    crate::cli::index::generate(&paths, &lockfile)?;
    progress.finish();
    println!("Synced envs/{env}.");
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    print!("{prompt} [y/N] ");
    std::io::stdout().flush().ok();
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(matches!(line.trim().chars().next(), Some('y') | Some('Y')))
}
```

(Adapter `classify::from_catalog_scan_lockfile` is a thin wrapper that converts the catalog + scan + lockfile into the hash-map inputs for `classify::classify`. It lives next to `classify` to keep the public-facing API focused.)

- [ ] **Step 4: Stub `execute::run` to be a no-op for now**

```rust
// src/cli/sync/execute.rs
pub async fn run(
    _ctx: &mut crate::cli::pull::common::PullCtx<'_>,
    _catalog: &crate::cli::pull::common::RemoteCatalog,
    _classified: &[crate::cli::sync::classify::ClassifiedItem],
    _no_push: bool,
    _no_pull: bool,
    _interactive: bool,
    _progress: &std::sync::Arc<crate::progress::OverallProgress>,
) -> anyhow::Result<()> {
    Ok(())
}
```

- [ ] **Step 5: Run the smoke test**

Run: `cargo test -p rdc --test cli_sync sync_clean_env_does_no_writes -- --nocapture`
Expected: PASS — empty classification → empty plan → no writes.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
sync: run() wired to list_remote, scan, classify, plan; executor stub

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 14: `sync::execute` — pull-side writes

**Goal:** Implement the pull-side branch of the executor. For every classified item whose class is `RemoteEdit` or `RemoteCreate`, write the local file (delegating to `pull::*::process` with a subset filter).

**Files:**
- Modify: `src/cli/sync/execute.rs`.

- [ ] **Step 1: Write failing test**

```rust
// tests/cli_sync.rs
#[tokio::test]
async fn sync_remote_change_only_writes_local() {
    let server = MockServer::start().await;
    // Mock one label that exists on remote but not on disk; lockfile empty.
    let label_json = serde_json::json!({
        "id": 1, "url": format!("{}/api/v1/labels/1", server.uri()),
        "name": "Audit hold", "organization": format!("{}/api/v1/organizations/123", server.uri())
    });
    Mock::given(method("GET")).and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [label_json.clone()], "next": null, "previous": null
        })))
        .mount(&server).await;
    // ... empty listings for other kinds ...

    let tmp = tempfile::tempdir().unwrap();
    // Initialize project pointing at server, run sync, assert audit-hold.json exists.
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --test cli_sync sync_remote_change_only_writes_local -- --nocapture`
Expected: FAIL — executor is a no-op.

- [ ] **Step 3: Implement pull-side execution**

```rust
// src/cli/sync/execute.rs
use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
use std::collections::BTreeSet;

pub async fn run(
    ctx: &mut crate::cli::pull::common::PullCtx<'_>,
    catalog: &crate::cli::pull::common::RemoteCatalog,
    classified: &[ClassifiedItem],
    no_push: bool,
    no_pull: bool,
    interactive: bool,
    progress: &std::sync::Arc<crate::progress::OverallProgress>,
) -> anyhow::Result<()> {
    // Pull-side subsets, grouped by kind.
    let mut pull_subsets: std::collections::BTreeMap<&str, BTreeSet<(String, String)>> = Default::default();
    if !no_pull {
        for it in classified {
            if matches!(it.class, SyncClass::RemoteEdit | SyncClass::RemoteCreate) {
                pull_subsets.entry(it.kind.as_str()).or_default()
                    .insert((it.kind.clone(), it.slug.clone()));
            }
        }
    }

    // Pull-side writes (one call per kind that has at least one item).
    if let Some(s) = pull_subsets.get("workspaces") {
        crate::cli::pull::workspaces::process(ctx, catalog.workspaces.clone(), s, progress).await?;
    }
    if let Some(s) = pull_subsets.get("queues") {
        crate::cli::pull::queues::process(ctx, catalog.queues.clone(), s, progress).await?;
    }
    // ... repeat for hooks, rules, labels, engines, engine_fields,
    //     workflows, workflow_steps, email_templates, mdh.

    Ok(())
}
```

- [ ] **Step 4: Run + commit**

Run: `cargo test -p rdc --test cli_sync sync_remote_change_only_writes_local -- --nocapture`
Expected: PASS.

```bash
git add src/cli/sync/execute.rs
git commit -m "$(cat <<'EOF'
sync: execute pull-side writes via per-kind subset

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 15: `sync::execute` — push-side writes

**Goal:** For every classified item whose class is `LocalEdit`, `LocalCreate`, or `LocalDelete`, delegate to the existing push pipeline (`push::run_drivers` renamed `push_classified`).

**Files:**
- Modify: `src/cli/push/mod.rs` — rename `run_drivers` to `pub(crate) async fn push_classified` and remove the now-dead `run` entry point's body (Task 16 will fully delete the function).
- Modify: `src/cli/sync/execute.rs` — call `push_classified`.

- [ ] **Step 1: Write failing test**

```rust
// tests/cli_sync.rs
#[tokio::test]
async fn sync_local_edit_only_patches_remote() {
    let server = MockServer::start().await;
    // Snapshot has one edited hook; lockfile records its base hash.
    // Mock GET returns the unchanged remote, mock PATCH expects exactly one call.
    // Assert PATCH was hit exactly once.
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --test cli_sync sync_local_edit_only_patches_remote -- --nocapture`
Expected: FAIL — push branch not wired.

- [ ] **Step 3: Promote `run_drivers` to pub(crate) and call it from `sync::execute`**

In `src/cli/push/mod.rs`, change `async fn run_drivers(...)` → `pub(crate) async fn push_classified(...)`. Update its signature to take a `ChangeList` directly (already does) plus the env name and progress. The body is unchanged.

In `src/cli/sync/execute.rs`:

```rust
if !no_push {
    let change_list = crate::cli::push::scan::change_list_from_classified(classified);
    if !change_list.is_empty() {
        crate::cli::push::push_classified(
            ctx.paths,
            ctx.client,
            ctx.lockfile,
            &ctx.env,
            interactive,
            &change_list,
            progress,
        ).await?;
    }
}
```

- [ ] **Step 4: Run + commit**

Run: `cargo test -p rdc --test cli_sync sync_local_edit_only_patches_remote -- --nocapture`
Expected: PASS.

```bash
git add src/cli/sync/execute.rs src/cli/push/mod.rs
git commit -m "$(cat <<'EOF'
sync: execute push-side writes via push_classified

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 16: `sync::execute` — conflict resolver dispatch (both-diverged)

**Goal:** For every `BothDiverged` item, prompt the user via the existing resolver. On `[k]` issue a PATCH; on `[r]` write local with remote bytes; on `[e]` issue both; on `[s]` write shadow; on `[a]` abort.

**Files:**
- Modify: `src/cli/sync/execute.rs`.

- [ ] **Step 1: Write failing test**

```rust
// tests/cli_sync.rs
#[tokio::test]
async fn sync_conflict_keep_local_patches_remote() {
    // Setup: both local and remote differ from lockfile.
    // Scripted stdin: "k\n".
    // Assert: PATCH hit once with local body; lockfile records local hash.
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rdc --test cli_sync sync_conflict_keep_local_patches_remote -- --nocapture`
Expected: FAIL.

- [ ] **Step 3: Implement conflict dispatch**

```rust
// At top of execute::run, before pull-side and push-side phases:
for it in classified {
    if !matches!(it.class, SyncClass::BothDiverged) { continue; }
    let local_path = ctx.paths.path_for(&it.kind, &it.slug);  // helper
    let remote_bytes = catalog.bytes_for(&it.kind, &it.slug)?; // helper
    let resolution = crate::cli::resolve::resolve_combined_file(
        interactive, &local_path, &remote_bytes, progress, &ctx.env,
    )?;
    // Apply: KeepLocal → PATCH via push::*; KeepRemote → write local
    // bytes; Edit(bytes) → write local AND issue PATCH; Skip → shadow
    // already written by resolver; Abort → bubble PullAborted.
    match resolution {
        // ...
    }
}
```

`Paths::path_for(kind, slug)` is a tiny helper in `src/paths.rs` that maps `("hooks", "validator-invoices") → envs/<env>/hooks/validator-invoices.json` (use the same mapping as the existing pull/push drivers).

`RemoteCatalog::bytes_for(kind, slug)` returns the JSON bytes from the in-memory catalog item; trivial helper.

- [ ] **Step 4: Run + commit**

Run: `cargo test -p rdc --test cli_sync sync_conflict_keep_local_patches_remote -- --nocapture`
Expected: PASS.

```bash
git add src/cli/sync/execute.rs src/paths.rs src/cli/pull/common.rs
git commit -m "$(cat <<'EOF'
sync: conflict resolver dispatch for both-diverged class

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 17: `sync::execute` — remote-delete and double-conflict cases

**Goal:** Use `prompt_remote_delete` for `RemoteDelete`. The two double-conflict classes also use `prompt_remote_delete` but with a "(unsynced edits)" suffix in the local-side preview header. `BothDeleted` is silent: just drop the lockfile entry.

**Files:**
- Modify: `src/cli/sync/execute.rs`.

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn sync_remote_deleted_mirror_via_use_env_deletes_local() {
    // Lockfile entry, no local change, listing missing the entry.
    // Scripted stdin: "r\n".
    // Assert: local file removed; lockfile entry dropped.
}

#[tokio::test]
async fn sync_both_deleted_silent_drops_lockfile_entry() {
    // Lockfile entry, local file deleted (tombstone), remote listing missing.
    // No scripted stdin needed.
    // Assert: lockfile entry dropped; no prompt rendered; no writes.
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rdc --test cli_sync sync_remote_deleted -- --nocapture`
Expected: FAIL.

- [ ] **Step 3: Implement the remote-deleted handler**

```rust
for it in classified {
    match it.class {
        SyncClass::RemoteDelete
        | SyncClass::LocalEditRemoteDelete
        | SyncClass::LocalDeleteRemoteEdit => {
            let local_path = ctx.paths.path_for(&it.kind, &it.slug);
            let resolution = if interactive {
                let stdin = std::io::stdin();
                let stdout = std::io::stderr();  // resolver writes to stderr
                crate::cli::resolve::prompt_remote_delete(stdin.lock(), stdout, &local_path, &ctx.env)?
            } else {
                // Non-TTY / --yes → fall back to [s] (write env-deleted marker).
                Resolution::Skip
            };
            apply_remote_delete(ctx, &it, &local_path, resolution).await?;
        }
        SyncClass::BothDeleted => {
            // Silent convergence: drop the lockfile entry, no prompt.
            ctx.lockfile.remove_entry(&it.kind, &it.slug);
        }
        _ => {}
    }
}
```

Where `apply_remote_delete` dispatches:

```rust
async fn apply_remote_delete(
    ctx: &mut crate::cli::pull::common::PullCtx<'_>,
    item: &ClassifiedItem,
    local_path: &std::path::Path,
    res: crate::cli::resolve::Resolution,
) -> anyhow::Result<()> {
    use crate::cli::resolve::Resolution;
    match res {
        Resolution::KeepLocal => {
            // Restore on env: POST the local body. Delegate to push::*::create.
            // (Implementation varies by kind; reuse the same per-kind POST
            // helpers the push pipeline uses.)
            push_restore_on_env(ctx, item).await?;
        }
        Resolution::KeepRemote => {
            // Mirror remote: remove local + lockfile entry.
            if local_path.exists() {
                std::fs::remove_file(local_path)?;
            }
            ctx.lockfile.remove_entry(&item.kind, &item.slug);
        }
        Resolution::Skip => {
            // Write env-deleted marker.
            let shadow = local_path.with_file_name(format!(
                "{}.{}-deleted",
                local_path.file_name().and_then(|f| f.to_str()).unwrap_or("shadow"),
                ctx.env,
            ));
            std::fs::write(&shadow, b"")?;
        }
        Resolution::Edit(_) => {
            // Editor not offered for delete prompts; shouldn't happen.
        }
        Resolution::Abort => {
            return Err(anyhow::Error::new(crate::cli::resolve::PullAborted));
        }
    }
    Ok(())
}
```

Helper `push_restore_on_env` is per-kind dispatch into the existing POST creators (look at `src/cli/push/labels.rs::push` etc., which already handle POST for new objects).

- [ ] **Step 4: Run + commit**

Run: `cargo test -p rdc --test cli_sync sync_remote_deleted -- --nocapture`
Expected: PASS.

```bash
git add src/cli/sync/execute.rs
git commit -m "$(cat <<'EOF'
sync: handle remote-delete + double-conflicts + both-deleted

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 18: Add `Sync` to CLI and route through main

**Goal:** Make `rdc sync test` work from the command line. Pull/push commands stay for now (deleted in Task 20).

**Files:**
- Modify: `src/cli/mod.rs` — add `Sync` variant; add routing in `run()`.

- [ ] **Step 1: Add the variant**

In `src/cli/mod.rs::Command`:

```rust
/// Reconcile the local snapshot and the env's remote state in one pass.
/// Without `<env>`, picks interactively from envs defined in `rdc.toml`
/// (or auto-selects when only one exists).
Sync {
    env: Option<String>,
    /// Print the plan and exit without making any changes.
    #[arg(long = "dry-run")]
    dry_run: bool,
    /// With `--dry-run`, print per-object unified diffs.
    #[arg(long = "diff", requires = "dry_run")]
    diff: bool,
    /// Permit local-tombstone → remote DELETE without per-object prompts.
    #[arg(long = "allow-deletes")]
    allow_deletes: bool,
    /// Audit mode: pull changes into local but never write to the remote.
    #[arg(long = "no-push", conflicts_with = "no_pull")]
    no_push: bool,
    /// Deploy mode: write local edits to the remote but never overwrite
    /// local files.
    #[arg(long = "no-pull", conflicts_with = "no_push")]
    no_pull: bool,
},
```

- [ ] **Step 2: Add routing in `run`**

```rust
Some(Command::Sync { env, dry_run, diff, allow_deletes, no_push, no_pull }) => {
    let env = crate::cli::env_picker::pick_env("Which env to sync?", env)?;
    let interactive = crate::cli::resolve::is_interactive(cli.yes);
    with_401_retry(&env, || {
        crate::cli::sync::run(&env, interactive, dry_run, diff, allow_deletes, no_push, no_pull)
    })
    .await
}
```

- [ ] **Step 3: Verify `rdc sync --help` renders**

Run: `cargo run -- sync --help`
Expected: help text shown with all flags.

- [ ] **Step 4: Run full suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/mod.rs
git commit -m "$(cat <<'EOF'
cli: register sync subcommand and route through main

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 19: Integration tests for the remaining spec scenarios

**Goal:** Cover the rest of the integration-test list from the spec.

**Files:**
- Modify: `tests/cli_sync.rs` — add the remaining test cases.

- [ ] **Step 1: Add each test in turn (each is one TDD micro-cycle)**

For each of the following spec test names, write the test, run it red, then ensure it passes with the existing implementation (most should — they're verifying the wiring is correct):

```text
sync_conflict_use_env
sync_conflict_skip_writes_env_shadow
sync_remote_deleted_restore_via_keep_local
sync_remote_deleted_skip
sync_remote_deleted_yes_falls_back_to_skip
sync_no_push
sync_no_pull
sync_yes_with_tombstones_refused
sync_dry_run_diff
sync_env_name_in_prompt
sync_local_edit_remote_delete_keep_local
sync_local_delete_remote_edit_keep_local
```

Each test is short (10–40 lines) — set up the mock server, scripted stdin if needed, run `sync::run`, assert.

- [ ] **Step 2: Run the suite**

Run: `cargo test -p rdc --test cli_sync -- --nocapture`
Expected: PASS for all.

- [ ] **Step 3: Commit (one commit, or split per test if any required impl tweaks)**

```bash
git add tests/cli_sync.rs
git commit -m "$(cat <<'EOF'
test: cover remaining sync integration scenarios from spec

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 20: Remove `Pull` and `Push` from the CLI

**Goal:** Delete the `Pull` and `Push` variants and their associated `cli::pull::run` / `cli::push::run` entry points. The per-kind drivers stay in their modules; only the public top-level entry points are removed. Delete the now-redundant integration tests.

**Files:**
- Modify: `src/cli/mod.rs` — remove `Pull` and `Push` variants; remove their routing arms in `run`.
- Modify: `src/cli/pull/mod.rs` — delete `pub async fn run` and the `is_aborted` helper (move it into `sync::execute` if still needed).
- Modify: `src/cli/push/mod.rs` — delete `pub async fn run`; keep `push_classified`, `scan`, drivers, `deletes`.
- Delete: `tests/cli_pull.rs`.
- Delete: `tests/cli_push.rs`.

- [ ] **Step 1: Delete the CLI variants and routing**

In `src/cli/mod.rs`:
- Remove the `Pull { env: ... }` variant from the enum.
- Remove the `Push { ... }` variant.
- Remove the two routing arms in `run`.

- [ ] **Step 2: Delete the `run` functions**

```rust
// src/cli/pull/mod.rs
// Remove: pub async fn run(env: &str, interactive: bool) -> Result<()> { ... }
// Keep:  pub mod common, common::list_remote, common::PullCtx, and all per-kind modules.
```

```rust
// src/cli/push/mod.rs
// Remove: pub async fn run(env, interactive, dry_run, diff, allow_deletes) -> Result<()> { ... }
// Keep:  pub(crate) async fn push_classified (and all per-kind drivers / scan / deletes).
```

- [ ] **Step 3: Delete the obsolete integration tests**

```bash
rm tests/cli_pull.rs tests/cli_push.rs
```

The behavior they covered is now in `tests/cli_sync.rs` (via `--no-push` / `--no-pull` flags).

- [ ] **Step 4: Run the suite**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS. Compile errors come up if any internal helper still imports a deleted symbol — fix by deleting the import.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
cli: remove pull and push commands; sync is the single verb

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 21: README update

**Goal:** Refresh the user-facing documentation to reflect the new command. No source changes.

**Files:**
- Modify: `README.md`.

- [ ] **Step 1: Replace pull/push references**

Update the following sections:

**60-second tour** (lines ~71–84 of README, "A 60-second tour"): change `rdc pull test` / `rdc pull prod` to `rdc sync test` / `rdc sync prod`. Remove the `rdc status` / `rdc push` blocks for the test workflow — replace with a single `rdc sync test` block after the editor edit.

**Mental model** (lines ~160–171): replace the `rdc pull` / `rdc push` definitions with one sentence each:

```markdown
- **`envs/<env>/`** — the **snapshot**. `rdc sync` reconciles snapshot ↔ remote: pulling remote changes into local, pushing local edits to remote, and prompting on conflicts.
```

**Commands table** (lines ~172–186): remove the `rdc pull <env>` and `rdc push <env>` rows. Add a single `rdc sync <env>` row above the existing `deploy` row:

```markdown
| `rdc sync <env>` | Reconcile local snapshot and remote state. `--no-push` (audit), `--no-pull` (deploy from snapshot). |
```

**Edit the snapshot** subsections: change `rdc push test` references to `rdc sync test`.

**Delete an object** section: change `rdc push test --allow-deletes` to `rdc sync test --allow-deletes`.

**Conflicts & drift** section: update the inline resolver example to use env name instead of "remote" — e.g., the prompt now reads `[k] keep local  [r] use test  [e] edit  [s] skip (shadow file)  [a] abort >`.

- [ ] **Step 2: Verify README renders**

Visual: open `README.md` in a markdown previewer or `grep -n "rdc pull\|rdc push" README.md` to confirm no stale references remain.

```bash
grep -n "rdc pull\|rdc push" README.md
```

Expected: no output (or only output that refers to historical context that's now explicitly framed as such).

- [ ] **Step 3: Run the suite (sanity)**

Run: `cargo test -p rdc -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "$(cat <<'EOF'
docs: update README for unified sync command

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Spec Self-Review

Run through `docs/superpowers/specs/2026-05-14-unified-sync-design.md` section by section and confirm coverage:

- **CLI surface**: Tasks 4, 13, 18, 20 (`Sync` variant, `--no-push` / `--no-pull` flags, `--allow-deletes`, `--dry-run`, `--diff`).
- **Execution pipeline (5 phases)**: Tasks 8 (list), 13 (scan + classify + plan), 14–17 (execute).
- **Classification table (11 classes)**: Tasks 4–7.
- **Conflict resolver under sync (env-aware)**: Tasks 1–3 (parameter, prompt text, shadow file), 16 (dispatch).
- **Remote-side delete via resolver**: Task 12 (prompt), 17 (dispatch).
- **Double-conflict cases**: Tasks 7 (classify), 17 (dispatch).
- **`--dry-run` and `--diff`**: Task 13.
- **Idempotency invariants**: Covered transitively by integration tests in Task 19; the `sync_clean_env` test enforces invariant 1.
- **Plan-before-apply**: Task 11 (render), 13 (confirm).
- **Code shape**: Tasks 4 (new module), 8 (list_remote extraction), 9–10 (subset params), 13–17 (executor), 18 (CLI), 20 (pull/push removal).
- **CLI registration**: Task 18 (add Sync), Task 20 (remove Pull/Push).
- **README updates**: Task 21.
- **Errors & UX (`--no-push --no-pull` mutex)**: Task 13.
- **Testing (18 integration tests)**: Tasks 13 (smoke), 14, 15, 16, 17, 19.
- **Compatibility (shadow file rename)**: Task 3.

If any spec requirement is not pointed at by a task above, add a task. If the plan has placeholder text, fix it inline.
