# rdc M7 — Three-Way Pull Conflict Detection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop blindly overwriting local edits on subsequent pulls. Use the lockfile's `content_hash` (populated since M2) to detect when local AND remote both diverged from the last-pulled state. On real conflicts, preserve the local file and write `<slug>.json.remote` next to it for inspection. Surface conflict counts in the pull summary so users notice.

**Architecture:** A single shared decision helper consumes `(local_path, lockfile_entry, proposed_remote_bytes)` and returns a `PullAction` (Write / KeepLocal / Conflict). Each flat-list driver pre-serializes its model bytes (split from the existing `write_<kind>` codec function via a new `serialize_<kind>` companion), consults the helper, and acts accordingly. The lockfile's `content_hash` field IS the merge "base" — no separate cache directory needed.

**Tech Stack:** Same as M6.

**Scope of this milestone:**
- ✅ Three-way detection for **flat-list kinds**: hooks, organization, rules, labels, engines, engine_fields, workflows, workflow_steps, email_templates
- ❌ NOT yet for nested kinds (queues, schemas, inboxes, MDH) — they stay always-overwrite. Schemas in particular have the documented content_hash gap (formulas not covered); enabling 3-way for them requires the M5-documented combined-hash algorithm. Queues/inboxes are coupled to schemas and follow the same caveat. MDH ditto. M8 picks these up.
- ❌ NOT semantic merging (schema content[] by id, hook queues as set). M8 + M9 will add resolution UI; M7 is detection-only.

**End state of M7:**

```
$ rdc pull dev
Pulled ..., 2 conflicts from env 'dev'
warning: hooks/validator-invoices.json conflict — local preserved, remote at hooks/validator-invoices.json.remote
warning: rules/e-invoice-validation.json conflict — local preserved, remote at rules/e-invoice-validation.json.remote
```

Existing `rdc pull` invocations on a brand-new project keep working unchanged (no lockfile entries → all writes are first-pulls).

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `src/cli/pull/common.rs` | Modify | Add `PullAction` enum + `decide_pull_action` helper + `apply_pull_action` helper that handles file I/O |
| `src/cli/pull/mod.rs` | Modify | Sum conflict counts; surface in summary line and as warnings |
| `src/cli/pull/hooks.rs` | Modify | Use new helper |
| `src/cli/pull/organization.rs` | Modify | Use new helper |
| `src/cli/pull/rules.rs` | Modify | Use new helper |
| `src/cli/pull/labels.rs` | Modify | Use new helper |
| `src/cli/pull/engines.rs` | Modify | Use new helper |
| `src/cli/pull/engine_fields.rs` | Modify | Use new helper |
| `src/cli/pull/workflows.rs` | Modify | Use new helper |
| `src/cli/pull/workflow_steps.rs` | Modify | Use new helper |
| `src/cli/pull/email_templates.rs` | Modify | Use new helper |
| `tests/cli_pull.rs` | Modify | New tests for re-pull-noop, local-preserved, remote-applied, conflict-emits-remote |
| `README.md` | Modify | Note M7 scope |

Note: codec files do NOT need to change. The existing `write_<kind>(...) -> Result<Vec<u8>>` already returns the bytes; the new helper takes those bytes as the "proposed remote" without re-doing the serialization. Side-files (.py for hooks, formulas/ for schemas) are out of scope; the helper only governs the JSON file's write decision.

---

## Task 1: `PullAction` + decision helper

**Files:**
- Modify: `src/cli/pull/common.rs`

- [ ] **Step 1: Extend `common.rs` with the new types and functions**

Add to `src/cli/pull/common.rs` (after the existing `parse_id_from_url`):

```rust
use std::path::Path;

/// Outcome of a three-way comparison for a single object on pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullAction {
    /// First pull, or local hasn't been edited, or remote is unchanged from base —
    /// safe to write the remote bytes. The driver writes and updates lockfile.
    Write,
    /// Local has edits and remote is unchanged from base — keep the local file as-is.
    /// The driver does NOT overwrite; lockfile keeps the previous content_hash.
    KeepLocal,
    /// Both local and remote have diverged from base — real conflict.
    /// The driver writes `<path>.remote` (preserving local) and counts the conflict.
    Conflict,
}

/// Compute SHA-256 of bytes; convenience wrapper.
fn sha256_hex(bytes: &[u8]) -> String {
    crate::state::content_hash(bytes)
}

/// Decide what to do on pull for a single object.
///
/// `local_path` — the on-disk JSON file path (may not exist).
/// `base_hash` — the lockfile's recorded content_hash for this object (None if no prior entry).
/// `remote_bytes` — the just-serialized remote candidate bytes that would be written.
///
/// Returns: `(action, remote_hash)`. The remote_hash is always returned because the
/// caller needs it for the lockfile, regardless of which action is taken.
pub fn decide_pull_action(
    local_path: &Path,
    base_hash: Option<&str>,
    remote_bytes: &[u8],
) -> std::io::Result<(PullAction, String)> {
    let remote_hash = sha256_hex(remote_bytes);

    // No lockfile entry → first pull (or lockfile was wiped). Write unconditionally.
    let Some(base) = base_hash else {
        return Ok((PullAction::Write, remote_hash));
    };

    // No local file → nothing to preserve. Write.
    if !local_path.exists() {
        return Ok((PullAction::Write, remote_hash));
    }

    let local_bytes = std::fs::read(local_path)?;
    let local_hash = sha256_hex(&local_bytes);

    let local_matches_base = local_hash == base;
    let remote_matches_base = remote_hash == base;

    let action = match (local_matches_base, remote_matches_base) {
        (true, true) => PullAction::Write,        // No-op write (idempotent); both unchanged
        (true, false) => PullAction::Write,       // Server changed; safe to overwrite
        (false, true) => PullAction::KeepLocal,   // Local edits + remote unchanged
        (false, false) => PullAction::Conflict,   // Both diverged
    };

    Ok((action, remote_hash))
}

/// Apply the decision to the filesystem and return the hash that should be
/// recorded in the lockfile (which differs depending on the action).
///
/// On Write: writes `local_path` with `remote_bytes`. Returns `remote_hash`.
/// On KeepLocal: does nothing on disk. Returns the LOCAL file's hash so the
///   lockfile reflects what's actually on disk (not what the API returned).
/// On Conflict: writes `<local_path>.remote` with `remote_bytes`. Returns the
///   LOCAL file's hash (local is canonical until resolved).
pub fn apply_pull_action(
    action: PullAction,
    local_path: &Path,
    remote_bytes: &[u8],
    remote_hash: String,
) -> std::io::Result<String> {
    use crate::snapshot::writer::write_atomic;
    match action {
        PullAction::Write => {
            // M7: caller is responsible for parent dir creation (kept consistent
            // with existing driver patterns). write_atomic also handles it.
            write_atomic(local_path, remote_bytes).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, format!("{e:#}"))
            })?;
            Ok(remote_hash)
        }
        PullAction::KeepLocal => {
            let local_bytes = std::fs::read(local_path)?;
            Ok(sha256_hex(&local_bytes))
        }
        PullAction::Conflict => {
            // Path with `.remote` appended (e.g. `validator-invoices.json.remote`).
            let mut conflict_path = local_path.to_path_buf();
            let new_name = match conflict_path.file_name().and_then(|s| s.to_str()) {
                Some(name) => format!("{name}.remote"),
                None => "remote".to_string(),
            };
            conflict_path.set_file_name(new_name);
            write_atomic(&conflict_path, remote_bytes).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, format!("{e:#}"))
            })?;
            // Local stays canonical until resolved.
            let local_bytes = std::fs::read(local_path)?;
            Ok(sha256_hex(&local_bytes))
        }
    }
}
```

- [ ] **Step 2: Add unit tests for the helper**

Append to the `mod tests` block in `src/cli/pull/common.rs`:

```rust
    #[test]
    fn first_pull_writes_when_no_base_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let (action, _hash) = decide_pull_action(&path, None, b"{}").unwrap();
        assert_eq!(action, PullAction::Write);
    }

    #[test]
    fn write_when_no_local_file_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let (action, _hash) = decide_pull_action(&path, Some("any-hash"), b"{}").unwrap();
        assert_eq!(action, PullAction::Write);
    }

    #[test]
    fn keep_local_when_only_local_edited() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{ \"local\": true }").unwrap();
        let remote = b"{}";
        // base hash matches the bytes that local USED to be (we pretend remote == base).
        let base = sha256_hex(remote);
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::KeepLocal);
    }

    #[test]
    fn write_when_only_remote_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let original = b"{ \"original\": true }";
        std::fs::write(&path, original).unwrap();
        let base = sha256_hex(original);
        let remote = b"{ \"updated\": true }";
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::Write);
    }

    #[test]
    fn conflict_when_both_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{ \"local\": true }").unwrap();
        let base = "0".repeat(64);
        let remote = b"{ \"remote\": true }";
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::Conflict);
    }

    #[test]
    fn apply_write_creates_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let h = apply_pull_action(PullAction::Write, &path, b"hello", "h".repeat(64)).unwrap();
        assert_eq!(h, "h".repeat(64));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn apply_conflict_writes_remote_sibling() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local").unwrap();
        let _ = apply_pull_action(PullAction::Conflict, &path, b"remote", "h".repeat(64)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"local");
        assert_eq!(std::fs::read(dir.path().join("x.json.remote")).unwrap(), b"remote");
    }
```

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib cli::pull::common`
Expected: 13 tests pass (5 from M5 + 7 new + 1 existing pluralize, etc.). Use the actual count from the output.

- [ ] **Step 4: Commit**

```bash
git add src/cli/pull/common.rs
git commit -m "feat(cli): three-way pull decision helper with PullAction enum"
```

---

## Task 2: Wire flat-list drivers to use the helper

Apply a consistent transformation to all 9 flat-list drivers. Each driver currently does:

```rust
let bytes = write_<kind>(dir, slug, item)?;
let hash = hash_for_lockfile(&bytes);
record_object(ctx.lockfile, kind, slug, ..., Some(hash));
```

After this task, each driver does:

```rust
// 1. Compute proposed remote bytes (same serialization, same path).
//    write_<kind> already does this — but we need to NOT write yet.
//    Solution: call write_<kind> to get the bytes, then call decide_pull_action
//    with what's on disk (write_<kind> always overwrites; we accept that as the
//    M7 simplification because for first pulls the old and new bytes are
//    semantically equivalent and decide_pull_action rules apply uniformly).
//
// Actually, simpler: write_<kind> writes AND returns bytes. We compute the
// decision AFTER the write and only emit the .remote file in the conflict case.
// On Conflict, we restore the local file from a pre-write read.
```

**Revised approach** — to avoid the read-before-write race entirely, we do this in each driver:

```rust
let local_path = dir.join(format!("{slug}.json"));
let pre_local = if local_path.exists() {
    Some(std::fs::read(&local_path)?)
} else {
    None
};

let proposed_bytes = write_<kind>(dir, slug, item)?;  // writes file
let proposed_hash = hash_for_lockfile(&proposed_bytes);

let base_hash = ctx.lockfile.objects
    .get(kind)
    .and_then(|m| m.get(slug))
    .and_then(|e| e.content_hash.as_deref());

// Determine action against pre-write local state.
let action = match (base_hash, &pre_local) {
    (None, _) => PullAction::Write,  // first pull
    (_, None) => PullAction::Write,  // no pre-existing local
    (Some(base), Some(local)) => {
        let local_hash = content_hash(local);
        let local_matches_base = local_hash == base;
        let remote_matches_base = proposed_hash == base;
        match (local_matches_base, remote_matches_base) {
            (true, _) | (_, true) => PullAction::Write, // no-op or only-server-changed; write is fine (already done)
            (false, false) => PullAction::Conflict,
        }
    }
};

let recorded_hash = match action {
    PullAction::Write => proposed_hash,
    PullAction::KeepLocal => {
        // Restore local; record local hash.
        let local = pre_local.unwrap();
        write_atomic(&local_path, &local)?;
        content_hash(&local)
    }
    PullAction::Conflict => {
        // Restore local; rename what we just wrote to <path>.remote.
        let local = pre_local.unwrap();
        write_atomic(&local_path, &local)?;
        let conflict_path = local_path.with_file_name(format!("{slug}.json.remote"));
        write_atomic(&conflict_path, &proposed_bytes)?;
        conflict_count += 1;
        content_hash(&local)
    }
};

record_object(..., Some(recorded_hash));
```

Hmm — but this means we write the proposed bytes, then potentially overwrite them with the local. Two writes for the conflict path. Cleaner approach: serialize to bytes WITHOUT writing first, then decide, then write only what's needed.

To avoid touching all 11 codecs to add `serialize_<kind>` companions, I'll wrap `write_<kind>` in a "dry-run" via `serialize_<kind>` helpers added to common.rs that re-implement the JSON serialization for each model type… no, that's worse.

**Final approach**: Update common.rs's `apply_pull_action` to take the codec write function as a closure. Then each driver does:

```rust
let proposed_bytes = serde_json::to_vec_pretty(item)?;
let mut proposed = proposed_bytes;
proposed.push(b'\n');
let local_path = dir.join(format!("{slug}.json"));
let base_hash = ctx.lockfile_get_hash(kind, slug);
let (action, remote_hash) = decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
let recorded_hash = match action {
    PullAction::Write | PullAction::KeepLocal | PullAction::Conflict => {
        apply_pull_action(action, &local_path, &proposed, remote_hash)?
    }
};
```

Each driver does its own JSON serialization. The codec's `write_<kind>` is no longer called for flat-list kinds; it stays available for tests and for nested-kind callers (queues, schemas, inboxes that aren't in M7 scope).

This is cleanest. Each flat-list driver's body becomes:

```rust
let local_path = ctx.paths.<kind>_dir().join(format!("{slug}.json"));
let mut proposed = serde_json::to_vec_pretty(item).context("serializing")?;
proposed.push(b'\n');
let base_hash = ctx.lockfile.objects.get("<kind>")
    .and_then(|m| m.get(&slug))
    .and_then(|e| e.content_hash.clone());
let (action, remote_hash) = decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
if let PullAction::Conflict = action { conflict_count += 1; }
let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash)?;
record_object(ctx.lockfile, "<kind>", &slug, item.id, Some(item.url.clone()),
              item.modified_at().map(|s| s.to_string()), Some(recorded_hash));
```

Drivers also need to track `conflict_count` and return it. Update the `pull` signature to return `(usize, usize)` = (count, conflicts).

The orchestrator sums conflicts and includes in summary.

**Files:**
- Modify: `src/cli/pull/hooks.rs`
- Modify: `src/cli/pull/organization.rs`
- Modify: `src/cli/pull/rules.rs`
- Modify: `src/cli/pull/labels.rs`
- Modify: `src/cli/pull/engines.rs`
- Modify: `src/cli/pull/engine_fields.rs`
- Modify: `src/cli/pull/workflows.rs`
- Modify: `src/cli/pull/workflow_steps.rs`
- Modify: `src/cli/pull/email_templates.rs`

- [ ] **Step 1: Update each flat-list driver**

Apply the pattern above to each driver. Driver signature changes from `pull(...) -> Result<usize>` to `pull(...) -> Result<(usize, usize)>` returning `(count, conflicts)`.

Hooks driver also needs to handle the `.py` file separately. For M7, the `.py` is always overwritten (no 3-way for it yet — documented limitation). After the JSON 3-way decision, write the .py file unconditionally if hook has code.

Concrete updates per file are mechanical — same pattern, different model type and kind name. Each driver follows the structure described in the "Final approach" block above.

Hooks-specific: after the JSON action, write the .py file using existing `write_hook` codec mechanics (or factor out the `.py` write to a small helper in `snapshot/hook.rs`). Simplest: keep calling `write_hook` then handle the JSON 3-way separately. But `write_hook` writes BOTH json and py — we want only py. Add `write_hook_code(dir, slug, code: &str)` to `src/snapshot/hook.rs` that just writes the .py.

For schema codec (already extracts formulas), no change in M7 (schemas are out of scope).

- [ ] **Step 2: Add `write_hook_code` to `src/snapshot/hook.rs`**

In `src/snapshot/hook.rs`, add a public function (after `write_hook`):

```rust
/// Write only the hook's `.py` file (extracted from `config.code`). Used by
/// pull drivers that compute the JSON write decision separately and only need
/// to overwrite the code file.
pub fn write_hook_code(dir: &Path, slug: &str, code: &str) -> Result<()> {
    let py_path = dir.join(format!("{slug}.py"));
    write_atomic(&py_path, code.as_bytes())?;
    Ok(())
}
```

- [ ] **Step 3: Update orchestrator to track conflicts**

In `src/cli/pull/mod.rs`, change every flat-list driver call from `let n = X::pull(&mut ctx).await?` to:

```rust
let (n_X, c_X) = X::pull(&mut ctx).await?;
```

Sum all `c_*` into `total_conflicts`. Append to summary if > 0:

```rust
if total_conflicts > 0 {
    summary.push_str(&format!(", {}", common::pluralize(total_conflicts, "conflict", "conflicts")));
}
```

After the summary, emit per-conflict warnings (collected as a `Vec<String>` from drivers, OR have drivers println directly when they emit a `.remote` file).

For simplicity: each driver `eprintln!("warning: <kind>/<slug>.json conflict — local preserved, remote at <kind>/<slug>.json.remote")` at the moment of writing the `.remote` file. The summary just shows the count.

- [ ] **Step 4: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all existing 131 tests still pass (existing fixtures all return non-empty results; lockfile didn't exist before pull, so all writes are first-pulls, which are `PullAction::Write`).

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat(cli): three-way conflict detection for flat-list pulls"
```

---

## Task 3: Integration tests for re-pull behavior

**Files:**
- Modify: `tests/cli_pull.rs`

- [ ] **Step 1: Add three new integration tests**

Append to `tests/cli_pull.rs`:

```rust
/// First pull writes everything; second pull with no changes is a clean no-op
/// (no overwrites, no conflicts).
#[tokio::test]
async fn re_pull_with_no_changes_is_idempotent() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/hooks", "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(&server)
            .await;
    }

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();

    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    // First pull
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success();

    let lf_path = project.path().join(".rdc/state/dev.lock.json");
    let first_lf = std::fs::read_to_string(&lf_path).unwrap();

    // Second pull
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("conflict").not());

    let second_lf = std::fs::read_to_string(&lf_path).unwrap();
    assert_eq!(first_lf, second_lf, "lockfile should be byte-identical after no-op re-pull");
}

/// Local edit + remote unchanged → local preserved (no overwrite).
#[tokio::test]
async fn re_pull_preserves_local_edits_when_remote_unchanged() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .mount(&server)
        .await;
    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(&server)
            .await;
    }

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();

    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    // First pull
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success();

    // Edit a hook locally
    let hook_path = project.path().join("envs/dev/hooks/validator-invoices.json");
    let original = std::fs::read_to_string(&hook_path).unwrap();
    let edited = original.replace("Validator: invoices", "Validator: invoices (LOCAL EDIT)");
    std::fs::write(&hook_path, &edited).unwrap();

    // Second pull (remote returns same content)
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("conflict").not());

    let after = std::fs::read_to_string(&hook_path).unwrap();
    assert_eq!(after, edited, "local edit must be preserved on re-pull when remote unchanged");
}

/// Local edit + remote changed = real conflict → local preserved + .remote file written.
#[tokio::test]
async fn re_pull_emits_remote_file_on_real_conflict() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    // First and second pulls return DIFFERENT hooks_list. Use a stateful mock.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
    CALL_COUNT.store(0, Ordering::SeqCst);

    let modified_hooks = serde_json::json!({
        "pagination": { "total": 2, "next": null, "previous": null },
        "results": [
            {
                "id": 1,
                "url": "https://mock.rossum.app/api/v1/hooks/1",
                "name": "Validator: invoices (REMOTE EDIT)",
                "type": "function",
                "queues": ["https://mock.rossum.app/api/v1/queues/100"],
                "events": ["annotation_content"],
                "config": { "runtime": "python3.12", "code": "def x(payload):\n    return {}\n" }
            },
            {
                "id": 2,
                "url": "https://mock.rossum.app/api/v1/hooks/2",
                "name": "SFTP import",
                "type": "function",
                "queues": [],
                "events": ["annotation_status"],
                "config": { "runtime": "python3.12", "code": "def import_files():\n    pass\n" }
            }
        ]
    });

    // We need to return original hooks first, then modified. wiremock allows
    // a `respond_with` closure or two mocks distinguished by number of times.
    // Simpler: use up_to_n_times.
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("hooks_list.json")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(modified_hooks))
        .mount(&server)
        .await;

    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues",
        "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(&server)
            .await;
    }

    let project = TempDir::new().unwrap();

    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();

    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    // First pull
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success();

    // Edit the hook locally (different change from what remote will return).
    let hook_path = project.path().join("envs/dev/hooks/validator-invoices.json");
    let original = std::fs::read_to_string(&hook_path).unwrap();
    let local_edit = original.replace("Validator: invoices", "Validator: invoices (LOCAL EDIT)");
    std::fs::write(&hook_path, &local_edit).unwrap();

    // Second pull (remote returns DIFFERENT change)
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 conflict"));

    // Local was preserved
    let after_local = std::fs::read_to_string(&hook_path).unwrap();
    assert_eq!(after_local, local_edit, "local must be preserved on conflict");

    // .remote file was written with remote content
    let remote_path = project.path().join("envs/dev/hooks/validator-invoices.json.remote");
    assert!(remote_path.exists(), "<slug>.json.remote should be written on conflict");
    let remote_content = std::fs::read_to_string(&remote_path).unwrap();
    assert!(remote_content.contains("REMOTE EDIT"), "remote file should contain remote content");
}
```

- [ ] **Step 2: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: ALL tests pass — adds 3 new cli_pull tests.

- [ ] **Step 3: Commit**

```bash
git add tests/cli_pull.rs
git commit -m "test(cli): integration tests for three-way pull (idempotent, preserve, conflict)"
```

---

## Task 4: README + memory

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update README**

In `README.md`, update the Status line to reflect M7. Append a "Conflict handling" section below Quick start:

```
## Conflict handling

`rdc pull` is now safe to re-run. The lockfile's `content_hash` is used as the
"base" for a three-way comparison:

- If you haven't edited the local file and the remote changed → write the remote.
- If you edited the local file and the remote is unchanged → keep your edit.
- If both you and the remote changed → preserve your local file and write the
  remote alongside as `<slug>.json.remote` for inspection. The pull summary
  reports the conflict count.

Three-way detection is currently active for hooks, organization, rules, labels,
engines, engine fields, workflows, workflow steps, and email templates.
Schemas, queues, inboxes, and MDH still always-overwrite — they will join in M8.
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: document M7 three-way pull and conflict handling"
```

---

## Self-Review

**Spec coverage:**
- §8 Conflict handling — partial: detection works for flat-list kinds, semantic merging deferred to M8
- §4.1 Predictability — re-pull is now idempotent (was always-overwrite before)
- §13 Error handling — conflict warnings are stderr; errors remain actionable

**Placeholder scan:** No "TBD"/"TODO" patterns.

**Type consistency:** `PullAction { Write | KeepLocal | Conflict }`, `decide_pull_action`, `apply_pull_action` all defined in Task 1, used consistently in Task 2.

**Scope check:** 4 tasks. Most of the work is mechanical driver updates (9 files); the design is one helper. No technical debt; deferred work (semantic merging, schema/queue/inbox/MDH) is documented.

---

## Next milestones

- **M8:** Conflict resolver TUI + indexer (`_index.md`); extend three-way to schemas/queues/inboxes/MDH using the M5-documented combined-hash for schemas.
- **M9:** `rdc push`.
- **M10–M13:** as previously listed.
