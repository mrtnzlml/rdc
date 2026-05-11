# Pull/Push UX Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rdc pull` / `rdc push` quiet, confident, and predictable: per-kind progress bars with ETAs, colorful conflict resolver, and noise-field suppression so server-managed timestamps no longer trigger spurious conflicts.

**Architecture:** Three independently shippable phases, each with its own commit cadence: (1) noise-field suppression at the hash-computation layer, (2) ANSI coloring in the conflict resolver, (3) `indicatif`-backed per-kind progress UX with TTY/log-mode auto-fallback. New modules `src/snapshot/noise.rs` and `src/progress.rs` carry the new infrastructure; pull/push driver changes follow established patterns.

**Tech Stack:** Rust 2021 edition, `serde_json` (existing), `sha2` (existing), `similar` (existing), new deps `indicatif = "0.17"` and `anstyle = "1"`.

**Three-phase shipping option:** Each phase is independently mergeable. Ship them one at a time or merge all together — tasks are ordered so each subsequent phase builds on earlier ones cleanly.

**Spec:** [`docs/superpowers/specs/2026-05-07-pull-push-progress-bar-design.md`](../specs/2026-05-07-pull-push-progress-bar-design.md)

---

## Phase 1 — Noise-Field Suppression

### Task 1: Create `src/snapshot/noise.rs` with strip helper

**Why:** Centralize the list of server-managed fields (`modified_at`, `modifier`) that shouldn't influence content_hash. Pure data transform with no I/O — easy to TDD.

**Files:**
- Create: `src/snapshot/noise.rs`
- Modify: `src/snapshot/mod.rs` (declare new submodule)

- [ ] **Step 1: Write the failing tests**

Create `src/snapshot/noise.rs` with the test module only first:

```rust
//! Server-managed JSON fields stripped from content_hash inputs.
//!
//! Rossum's API stamps fields like `modified_at` and `modifier` on every
//! server-side touch. Including them in `content_hash` produces false-positive
//! conflicts on re-pull. This module strips them at hash-computation time
//! only; the on-disk JSON keeps every field (matches API output, useful
//! in editor and `rdc diff`).
//!
//! The list is intentionally a code constant. Adding a field requires a
//! one-line code change with a rationale comment.

/// Top-level and nested JSON keys removed from the canonical projection
/// before content_hash is computed.
pub const NOISE_FIELDS: &[&str] = &["modified_at", "modifier"];

/// Walk `value` and remove any object key whose name is in NOISE_FIELDS.
/// Recurses into nested objects and arrays. Mutates in place.
pub fn strip_noise_fields(value: &mut serde_json::Value) {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strip_removes_top_level_modified_at() {
        let mut v = json!({"name": "x", "modified_at": "2026-01-01T00:00:00Z"});
        strip_noise_fields(&mut v);
        assert_eq!(v, json!({"name": "x"}));
    }

    #[test]
    fn strip_removes_modifier() {
        let mut v = json!({"name": "x", "modifier": "https://x/api/v1/users/1"});
        strip_noise_fields(&mut v);
        assert_eq!(v, json!({"name": "x"}));
    }

    #[test]
    fn strip_removes_nested_modified_at() {
        let mut v = json!({
            "name": "x",
            "child": {"modified_at": "2026-01-01T00:00:00Z", "kept": true}
        });
        strip_noise_fields(&mut v);
        assert_eq!(v, json!({"name": "x", "child": {"kept": true}}));
    }

    #[test]
    fn strip_handles_array_of_objects() {
        let mut v = json!({
            "items": [
                {"id": 1, "modified_at": "t1"},
                {"id": 2, "modified_at": "t2"}
            ]
        });
        strip_noise_fields(&mut v);
        assert_eq!(v, json!({"items": [{"id": 1}, {"id": 2}]}));
    }

    #[test]
    fn strip_leaves_other_fields_alone() {
        let mut v = json!({
            "id": 42,
            "url": "https://x/api/v1/labels/42",
            "name": "Audit",
            "metadata": {"foo": "bar"}
        });
        let original = v.clone();
        strip_noise_fields(&mut v);
        assert_eq!(v, original);
    }

    #[test]
    fn strip_no_op_on_primitives_and_empty() {
        let mut v = json!(42);
        strip_noise_fields(&mut v);
        assert_eq!(v, json!(42));
        let mut v = json!({});
        strip_noise_fields(&mut v);
        assert_eq!(v, json!({}));
        let mut v = json!([]);
        strip_noise_fields(&mut v);
        assert_eq!(v, json!([]));
    }
}
```

Add the module declaration to `src/snapshot/mod.rs`:

```rust
pub mod collection;
pub mod email_template;
pub mod engine;
pub mod engine_field;
pub mod hook;
pub mod inbox;
pub mod index_set;
pub mod label;
pub mod noise;
pub mod organization;
pub mod queue;
pub mod rule;
pub mod schema;
pub mod workflow;
pub mod workflow_step;
pub mod workspace;
pub mod writer;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib snapshot::noise`
Expected: FAIL — six tests panic with `not yet implemented` from `todo!()`.

- [ ] **Step 3: Implement `strip_noise_fields`**

Replace the `todo!()` body with:

```rust
pub fn strip_noise_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for field in NOISE_FIELDS {
                map.remove(*field);
            }
            for (_, child) in map.iter_mut() {
                strip_noise_fields(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                strip_noise_fields(item);
            }
        }
        _ => {}
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib snapshot::noise`
Expected: PASS — six tests succeed.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/noise.rs src/snapshot/mod.rs
git commit -m "$(cat <<'EOF'
feat(snapshot): introduce noise-field strip helper

NOISE_FIELDS constant + strip_noise_fields walker in src/snapshot/noise.rs.
Foundation for canonicalize_for_hash and conflict-detection noise suppression.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Add `canonicalize_for_hash` helper

**Why:** Wraps strip + canonical re-serialize so hash sites have a one-call surface. Falls back gracefully on non-JSON input (some `content_hash` callers and tests pass raw bytes).

**Files:**
- Modify: `src/snapshot/noise.rs`

- [ ] **Step 1: Write the failing tests**

Append to `src/snapshot/noise.rs` test module:

```rust
    #[test]
    fn canonicalize_strips_modified_at() {
        let with = b"{\"name\":\"x\",\"modified_at\":\"t\"}";
        let without = b"{\"name\":\"x\"}";
        let c1 = canonicalize_for_hash(with);
        let c2 = canonicalize_for_hash(without);
        assert_eq!(c1, c2);
    }

    #[test]
    fn canonicalize_falls_back_on_non_json_bytes() {
        let raw = b"hello";
        let out = canonicalize_for_hash(raw);
        assert_eq!(out, raw.to_vec());
    }

    #[test]
    fn canonicalize_real_content_change_differs() {
        let a = b"{\"name\":\"foo\",\"modified_at\":\"t\"}";
        let b = b"{\"name\":\"bar\",\"modified_at\":\"t\"}";
        assert_ne!(canonicalize_for_hash(a), canonicalize_for_hash(b));
    }

    #[test]
    fn canonicalize_modifier_only_difference_collapses() {
        let a = b"{\"name\":\"x\",\"modifier\":\"u1\"}";
        let b = b"{\"name\":\"x\",\"modifier\":\"u2\"}";
        assert_eq!(canonicalize_for_hash(a), canonicalize_for_hash(b));
    }
```

Add the function signature (above the `#[cfg(test)]` block):

```rust
/// Produce a canonical byte projection of `bytes` for hashing:
/// parse as JSON, strip noise fields, re-serialize. Returns `bytes`
/// unchanged if parsing fails (e.g., non-JSON inputs from tests or
/// raw formula bytes used inside combined hashes).
pub fn canonicalize_for_hash(bytes: &[u8]) -> Vec<u8> {
    todo!()
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib snapshot::noise::tests::canonicalize`
Expected: FAIL — four `canonicalize_*` tests panic with `not yet implemented`.

- [ ] **Step 3: Implement `canonicalize_for_hash`**

```rust
pub fn canonicalize_for_hash(bytes: &[u8]) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    strip_noise_fields(&mut value);
    serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib snapshot::noise`
Expected: PASS — all ten tests succeed.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/noise.rs
git commit -m "$(cat <<'EOF'
feat(snapshot): add canonicalize_for_hash for JSON noise-field stripping

Parses, strips noise fields (modified_at/modifier), re-serializes. Falls
back to bytes-as-is on parse error so non-JSON callers (e.g. raw formula
bytes inside combined hashes) work unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Wire canonicalization into `content_hash` and combined-hash functions

**Why:** Single change point — every existing call site automatically gets noise-field-aware hashing without per-driver edits.

**Files:**
- Modify: `src/state/lockfile.rs:141-205` (the three hash functions)

- [ ] **Step 1: Write the failing tests**

Append to `src/state/lockfile.rs` test module (the one beginning around line 220 with `#[cfg(test)] mod tests`):

```rust
    #[test]
    fn content_hash_equal_when_only_modified_at_differs() {
        let a = b"{\"name\":\"x\",\"modified_at\":\"2026-01-01\"}";
        let b = b"{\"name\":\"x\",\"modified_at\":\"2026-12-31\"}";
        assert_eq!(content_hash(a), content_hash(b));
    }

    #[test]
    fn content_hash_differs_on_real_change() {
        let a = b"{\"name\":\"x\",\"modified_at\":\"t\"}";
        let b = b"{\"name\":\"y\",\"modified_at\":\"t\"}";
        assert_ne!(content_hash(a), content_hash(b));
    }

    #[test]
    fn content_hash_falls_back_for_non_json_bytes() {
        // Used in some tests and code paths that hash raw bytes.
        let h1 = content_hash(b"hello world");
        let h2 = content_hash(b"hello world");
        assert_eq!(h1, h2);
        assert_ne!(content_hash(b"hello world"), content_hash(b"goodbye"));
    }

    #[test]
    fn hook_combined_hash_strips_modified_at_in_json_portion() {
        let a = b"{\"name\":\"h\",\"modified_at\":\"t1\"}";
        let b = b"{\"name\":\"h\",\"modified_at\":\"t2\"}";
        let code = Some("def x(): pass".to_string());
        assert_eq!(hook_combined_hash(a, &code), hook_combined_hash(b, &code));
    }

    #[test]
    fn schema_combined_hash_strips_modified_at_in_json_portion() {
        let a = b"{\"name\":\"s\",\"modified_at\":\"t1\"}";
        let b = b"{\"name\":\"s\",\"modified_at\":\"t2\"}";
        let formulas = vec![("42".to_string(), b"return 1\n".to_vec())];
        assert_eq!(
            schema_combined_hash(a, &formulas),
            schema_combined_hash(b, &formulas)
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib state::lockfile::tests::content_hash_equal_when_only_modified_at_differs state::lockfile::tests::hook_combined_hash_strips state::lockfile::tests::schema_combined_hash_strips`
Expected: FAIL — three new tests fail because hashes still include `modified_at`.

- [ ] **Step 3: Update the three hash functions to canonicalize JSON inputs**

Edit `src/state/lockfile.rs:141`:

```rust
/// Compute a stable SHA-256 over canonical JSON bytes (with noise fields
/// stripped). Falls back to raw-byte SHA-256 for inputs that aren't valid
/// JSON. Hex-encoded output.
pub fn content_hash(bytes: &[u8]) -> String {
    let canonical = crate::snapshot::noise::canonicalize_for_hash(bytes);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        write!(&mut hex, "{:02x}", b).expect("writing to String cannot fail");
    }
    hex
}
```

Edit `src/state/lockfile.rs:167` (`schema_combined_hash`) — change only the first hasher.update call:

```rust
pub fn schema_combined_hash(json_bytes: &[u8], formulas: &[(String, Vec<u8>)]) -> String {
    let canonical = crate::snapshot::noise::canonicalize_for_hash(json_bytes);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    for (field_id, bytes) in formulas {
        hasher.update([0u8]);
        let path = format!("formulas/{field_id}.py");
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        write!(&mut hex, "{:02x}", b).expect("writing to String cannot fail");
    }
    hex
}
```

Edit `src/state/lockfile.rs:195` (`hook_combined_hash`):

```rust
pub fn hook_combined_hash(json_bytes: &[u8], code: &Option<String>) -> String {
    let canonical = crate::snapshot::noise::canonicalize_for_hash(json_bytes);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    if let Some(code) = code {
        hasher.update([0u8]);
        hasher.update(b"code");
        hasher.update([0u8]);
        hasher.update(code.as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        write!(&mut hex, "{:02x}", b).expect("writing to String cannot fail");
    }
    hex
}
```

- [ ] **Step 4: Run all tests to verify pass + no regressions**

Run: `cargo test --lib`
Expected: PASS — all existing lib tests + new five tests pass. (~250 tests; 4-5 new, since unchanged-input fixtures still hash deterministically.)

If any existing test fails because a fixture now hashes differently than the test asserted (e.g., a hard-coded hex hash literal in a test that included `modified_at`), update the literal. Most existing tests use either non-JSON bytes (fall-back path) or JSON without noise fields, so they won't be affected.

- [ ] **Step 5: Commit**

```bash
git add src/state/lockfile.rs
git commit -m "$(cat <<'EOF'
feat(state): canonicalize JSON inputs before content_hash / combined hashes

content_hash, schema_combined_hash, hook_combined_hash now strip noise fields
(modified_at, modifier) before hashing. Server-managed timestamp churn no
longer triggers false-positive conflicts on re-pull.

Backward compat: existing lockfiles will see one-time false conflicts on
first re-pull (hash-algorithm change). rdc repair --rebuild-lock clears.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Add `PullAction::NoChange` for byte-stable re-pulls

**Why:** When local + remote canonicalize equal, don't rewrite the file. Preserves the pull-twice-zero-diff invariant on disk (file mtime stays put when only `modified_at` server-churn would change).

**Files:**
- Modify: `src/cli/pull/common.rs:127-176` (PullAction enum, decide_pull_action) and `:186-212` (apply_pull_action)

- [ ] **Step 1: Write the failing test**

Append to `src/cli/pull/common.rs` test module (around line 278):

```rust
    #[test]
    fn decide_returns_nochange_when_canonical_local_equals_canonical_remote() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        // Local has modified_at = t1
        std::fs::write(&path, b"{\"name\":\"x\",\"modified_at\":\"t1\"}").unwrap();
        // Remote has modified_at = t2 (newer); same other content
        let remote = b"{\"name\":\"x\",\"modified_at\":\"t2\"}";
        // Base hash matches both (canonical strips modified_at)
        let base = content_hash(remote);
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::NoChange);
    }

    #[test]
    fn apply_nochange_does_not_modify_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"original").unwrap();
        let original_bytes = std::fs::read(&path).unwrap();
        let h = apply_pull_action(
            PullAction::NoChange,
            &path,
            b"different remote bytes",
            "h".repeat(64),
            false,
        )
        .unwrap();
        assert_eq!(h, "h".repeat(64));
        // Local file unchanged byte-for-byte.
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib pull::common::tests::decide_returns_nochange pull::common::tests::apply_nochange`
Expected: FAIL — `PullAction::NoChange` does not exist (compile error).

- [ ] **Step 3: Add the `NoChange` variant and short-circuit**

Edit `src/cli/pull/common.rs:128`:

```rust
/// Outcome of a three-way comparison for a single object on pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullAction {
    /// First pull, or local hasn't been edited, or remote is unchanged from base —
    /// safe to write the remote bytes.
    Write,
    /// Local has edits and remote is unchanged from base — keep the local file.
    KeepLocal,
    /// Both local and remote have diverged from base — real conflict.
    Conflict,
    /// Local and remote canonicalize to the same bytes (only noise fields
    /// like `modified_at` differ). Skip the write to preserve on-disk
    /// byte-stability across re-pulls.
    NoChange,
}
```

Edit `decide_pull_action` (replacing lines 146-176):

```rust
pub fn decide_pull_action(
    local_path: &Path,
    base_hash: Option<&str>,
    remote_bytes: &[u8],
) -> Result<(PullAction, String)> {
    let remote_hash = content_hash(remote_bytes);

    let Some(base) = base_hash else {
        return Ok((PullAction::Write, remote_hash));
    };

    if !local_path.exists() {
        return Ok((PullAction::Write, remote_hash));
    }

    let local_bytes = std::fs::read(local_path)
        .with_context(|| format!("reading {}", local_path.display()))?;
    let local_hash = content_hash(&local_bytes);

    // Short-circuit: canonicalized local == canonicalized remote means
    // any difference is noise (modified_at etc.). Don't rewrite the file.
    if local_hash == remote_hash {
        return Ok((PullAction::NoChange, remote_hash));
    }

    let local_matches_base = local_hash == base;
    let remote_matches_base = remote_hash == base;

    let action = match (local_matches_base, remote_matches_base) {
        (true, true) => PullAction::Write,
        (true, false) => PullAction::Write,
        (false, true) => PullAction::KeepLocal,
        (false, false) => PullAction::Conflict,
    };

    Ok((action, remote_hash))
}
```

Edit `apply_pull_action` (replacing lines 192-211):

```rust
pub fn apply_pull_action(
    action: PullAction,
    local_path: &Path,
    remote_bytes: &[u8],
    remote_hash: String,
    interactive: bool,
) -> Result<String> {
    use crate::snapshot::writer::write_atomic;
    match action {
        PullAction::Write => {
            write_atomic(local_path, remote_bytes)?;
            Ok(remote_hash)
        }
        PullAction::KeepLocal => {
            let local_bytes = std::fs::read(local_path)
                .with_context(|| format!("reading {}", local_path.display()))?;
            Ok(content_hash(&local_bytes))
        }
        PullAction::NoChange => {
            // Local and remote canonicalize equal — preserve disk bytes.
            // Hash is identical to remote_hash by construction.
            Ok(remote_hash)
        }
        PullAction::Conflict => {
            if interactive {
                resolve_conflict_interactive(local_path, remote_bytes, &remote_hash)
            } else {
                shadow_file_conflict(local_path, remote_bytes)
            }
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib pull::common`
Expected: PASS — including the two new `NoChange` tests and all eight existing tests.

- [ ] **Step 5: Commit**

```bash
git add src/cli/pull/common.rs
git commit -m "$(cat <<'EOF'
feat(pull): add PullAction::NoChange to skip writes when only noise differs

When canonicalized local equals canonicalized remote, the only difference is
noise fields (modified_at). Preserve on-disk byte-stability — no rewrite,
lockfile entry unchanged. pull-twice-zero-diff now holds across server
timestamp churn.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Strip noise in conflict resolver display

**Why:** When a real conflict surfaces, the user shouldn't see `modified_at` lines in the diff — they're noise. Strip before `unified_diff` runs.

**Files:**
- Modify: `src/cli/resolve.rs:88-134` (`prompt_resolve` function)

- [ ] **Step 1: Write the failing test**

Append to `src/cli/resolve.rs` test module (around line 344):

```rust
    #[test]
    fn prompt_short_circuits_when_only_noise_differs() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{\"name\":\"x\",\"modified_at\":\"t1\"}").unwrap();

        // Empty input — function must not block on read_line.
        let input = Cursor::new(b"");
        let mut output: Vec<u8> = Vec::new();
        let r = prompt_resolve(
            input,
            &mut output,
            1,
            1,
            &path,
            b"{\"name\":\"x\",\"modified_at\":\"t2\"}",
        )
        .unwrap();
        assert!(matches!(r, Resolution::KeepLocal));
        // No prompt was rendered (short-circuit).
        let s = String::from_utf8(output).unwrap();
        assert!(!s.contains("[k]eep"), "should not have prompted: {s}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli::resolve::tests::prompt_short_circuits_when_only_noise_differs`
Expected: FAIL — `unified_diff` produces a non-empty diff (raw bytes differ on `modified_at`), the resolver tries to prompt and reads EOF, returns `Skip` not `KeepLocal`.

- [ ] **Step 3: Apply canonicalization before `unified_diff`**

Edit the body of `prompt_resolve` in `src/cli/resolve.rs`. Replace the section starting at line 96:

```rust
pub fn prompt_resolve<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    remote_bytes: &[u8],
) -> Result<Resolution> {
    let local_bytes = read_local(local_path)?;

    // Strip noise fields before diff display so the user only sees real
    // changes. modified_at server-churn must not appear in the resolver.
    let local_canonical = crate::snapshot::noise::canonicalize_for_hash(&local_bytes);
    let remote_canonical = crate::snapshot::noise::canonicalize_for_hash(remote_bytes);

    if local_canonical == remote_canonical {
        // No meaningful difference — short-circuit before printing anything.
        return Ok(Resolution::KeepLocal);
    }

    writeln!(output, "")?;
    writeln!(output, "[{index}/{total}]  {} — conflict", local_path.display())?;
    writeln!(output, "")?;

    let diff = unified_diff("local", &local_canonical, "remote", &remote_canonical);
    if diff.is_empty() {
        // Defensive (canonicalize already short-circuited above).
        return Ok(Resolution::KeepLocal);
    }
    write!(output, "{diff}")?;
    writeln!(output, "")?;

    loop {
        write!(output, "[k]eep local  [r]emote  [e]dit  [s]kip (shadow file)  [a]bort > ")?;
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
            Some('e') | Some('E') => {
                let edited = run_editor_with_markers(&local_bytes, remote_bytes)?;
                return Ok(Resolution::Edit(edited));
            }
            _ => {
                writeln!(output, "  (unrecognized — pick one of k/r/e/s/a)")?;
                continue;
            }
        }
    }
}
```

Note: the editor still gets the *raw* `local_bytes` and `remote_bytes` (not canonical), so the user can preserve real on-disk content if they want.

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test --lib cli::resolve`
Expected: PASS — all eleven tests succeed (existing ten + new short-circuit test). The existing `prompt_keep_local_returns_keep_local` and friends still work because their fixtures don't have noise fields.

- [ ] **Step 5: Commit**

```bash
git add src/cli/resolve.rs
git commit -m "$(cat <<'EOF'
feat(resolve): canonicalize before diff display in conflict resolver

prompt_resolve now strips noise fields (modified_at, modifier) from both
sides before unified_diff runs. The resolver short-circuits to KeepLocal
when only noise differs, avoiding both spurious prompts and noisy diffs.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Integration test — pull twice with modified_at-only change

**Why:** End-to-end verification that the noise-field stack (Tasks 1-5) actually suppresses spurious conflicts caused by server-managed field churn.

**Files:**
- Modify: `tests/cli_pull.rs` (append new test)

- [ ] **Step 1: Open `tests/cli_pull.rs` and identify the existing fixture pattern**

Read it once: `cargo test --test cli_pull -- --list` and pick a small existing test (e.g., `pull_creates_hooks` or whichever asserts the lockfile content_hash). Match its fixture wiring (wiremock setup, project init, pull invocation).

- [ ] **Step 2: Add the new integration test**

Append to `tests/cli_pull.rs`:

```rust
#[tokio::test]
async fn pull_no_conflict_when_only_modified_at_differs() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let dir = tempfile::TempDir::new().unwrap();

    // Standard auth + minimal org/workspace/queue/etc fixture (copy from
    // an existing test in this file). Then mock /labels:
    let initial = serde_json::json!({
        "results": [{
            "id": 1,
            "url": format!("{}/api/v1/labels/1", server.uri()),
            "name": "audit-hold",
            "queues": [],
            "modified_at": "2026-01-01T00:00:00Z"
        }],
        "next": null
    });
    let _g = Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&initial))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    // First pull: writes label, lockfile records canonical hash.
    let mut cmd = assert_cmd::Command::cargo_bin("rdc").unwrap();
    cmd.args(["pull", "test"])
        .current_dir(dir.path())
        .assert()
        .success();

    drop(_g);

    // Second pull: same label, different modified_at.
    let bumped = serde_json::json!({
        "results": [{
            "id": 1,
            "url": format!("{}/api/v1/labels/1", server.uri()),
            "name": "audit-hold",
            "queues": [],
            "modified_at": "2026-12-31T23:59:59Z"
        }],
        "next": null
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&bumped))
        .mount(&server)
        .await;

    let mut cmd = assert_cmd::Command::cargo_bin("rdc").unwrap();
    let out = cmd
        .args(["pull", "test"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let stderr = String::from_utf8_lossy(&out.stderr);
    // No conflict count — second pull is a no-op for the label.
    assert!(
        !stderr.contains("conflict"),
        "expected no conflict on modified_at-only re-pull. stderr was: {stderr}"
    );

    // Disk-stability invariant: the local file is byte-identical to its
    // first-pull content (NoChange path skipped the write).
    let label_path = dir.path().join("envs/test/labels/audit-hold.json");
    let on_disk = std::fs::read_to_string(&label_path).unwrap();
    assert!(
        on_disk.contains("2026-01-01T00:00:00Z"),
        "expected on-disk file to retain first-pull modified_at. Found: {on_disk}"
    );
}
```

The fixture wiring (org/workspace/queues/etc) is omitted here because it varies across the existing tests in `tests/cli_pull.rs`. Match an existing test's pattern: `rdc init`, write `secrets/test.secrets.json`, mock the GETs the pull walks. The label-mock above is the diff-from-baseline.

- [ ] **Step 3: Run the new integration test**

Run: `cargo test --test cli_pull pull_no_conflict_when_only_modified_at_differs`
Expected: PASS.

If it fails because the fixture wiring is missing other GET endpoints, copy the wiring scaffolding from the nearest existing test in the file.

- [ ] **Step 4: Run the full test suite to verify no regressions**

Run: `cargo test`
Expected: PASS — all unit + integration tests succeed. ~10 new tests (5 noise unit + 4 hash unit + 2 PullAction unit + 1 resolver unit + 1 integration).

- [ ] **Step 5: Commit**

```bash
git add tests/cli_pull.rs
git commit -m "$(cat <<'EOF'
test(pull): integration test for modified_at-only re-pull

Verifies the noise-field stack: re-pull with same content but newer
modified_at produces no conflict, no warning, and preserves on-disk
file bytes (NoChange path).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

**Phase 1 checkpoint.** At this point noise-field suppression is fully shippable. Six commits. ~10 new tests. Continue with Phase 2 or merge as a standalone improvement.

---

## Phase 2 — Conflict Resolver Coloring

### Task 7: Add `anstyle` dependency

**Why:** Modern, NO_COLOR-aware ANSI styling crate; already a transitive dep of clap so the Cargo.lock add is small.

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add the dep**

Edit `Cargo.toml` `[dependencies]` block — insert (alphabetically):

```toml
[dependencies]
anstyle = "1"
anyhow = "1"
clap = { version = "4", features = ["derive"] }
futures = "0.3"
regex = "1"
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
similar = "2"
thiserror = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "fs"] }
toml = "0.8"
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build`
Expected: success. `Cargo.lock` updates with `anstyle` and any small transitive deps.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
chore: add anstyle dep for conflict resolver colors

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: Add color helpers in `src/cli/resolve.rs`

**Why:** Centralized style choices. Tests verify both color-on and color-off paths produce expected byte output.

**Files:**
- Modify: `src/cli/resolve.rs`

- [ ] **Step 1: Write failing tests**

Append to `src/cli/resolve.rs` test module:

```rust
    #[test]
    fn colorize_plain_mode_returns_unchanged() {
        let line = "-  \"name\": \"old\"";
        assert_eq!(colorize_diff_line(line, ColorMode::Plain), line.to_string());
    }

    #[test]
    fn colorize_color_mode_renders_minus_red() {
        let line = "-  \"name\": \"old\"";
        let out = colorize_diff_line(line, ColorMode::Color);
        // Bright-red SGR = \x1b[91m, reset = \x1b[0m.
        assert!(out.contains("\x1b[91m"), "expected bright red prefix in: {out:?}");
        assert!(out.ends_with("\x1b[0m"), "expected reset suffix in: {out:?}");
    }

    #[test]
    fn colorize_color_mode_renders_plus_green() {
        let line = "+  \"name\": \"new\"";
        let out = colorize_diff_line(line, ColorMode::Color);
        assert!(out.contains("\x1b[92m"), "expected bright green prefix in: {out:?}");
    }

    #[test]
    fn colorize_color_mode_leaves_context_lines_alone() {
        let line = "   \"unchanged\": true";
        assert_eq!(
            colorize_diff_line(line, ColorMode::Color),
            line.to_string()
        );
    }

    #[test]
    fn colorize_color_mode_hunk_header_is_cyan() {
        let line = "@@ -1,3 +1,3 @@";
        let out = colorize_diff_line(line, ColorMode::Color);
        assert!(out.contains("\x1b[36m"), "expected cyan in: {out:?}");
    }

    #[test]
    fn colorize_file_headers_are_red_and_green() {
        let minus_hdr = colorize_diff_line("--- local", ColorMode::Color);
        let plus_hdr = colorize_diff_line("+++ remote", ColorMode::Color);
        assert!(minus_hdr.contains("\x1b[91m"), "got: {minus_hdr:?}");
        assert!(plus_hdr.contains("\x1b[92m"), "got: {plus_hdr:?}");
    }

    #[test]
    fn detect_color_mode_no_color_env_returns_plain() {
        // Saved NO_COLOR is restored at end of test
        let prev = std::env::var("NO_COLOR").ok();
        std::env::set_var("NO_COLOR", "1");
        assert!(matches!(detect_color_mode(false), ColorMode::Plain));
        match prev {
            Some(v) => std::env::set_var("NO_COLOR", v),
            None => std::env::remove_var("NO_COLOR"),
        }
    }

    #[test]
    fn detect_color_mode_no_color_flag_returns_plain() {
        // --no-color flag forces Plain regardless of env.
        let prev = std::env::var("NO_COLOR").ok();
        std::env::remove_var("NO_COLOR");
        assert!(matches!(detect_color_mode(true), ColorMode::Plain));
        if let Some(v) = prev {
            std::env::set_var("NO_COLOR", v);
        }
    }
```

Add the public surface (above the `#[cfg(test)]` block):

```rust
/// Whether to emit ANSI color codes in resolver output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Plain,
    Color,
}

/// Decide the color mode at runtime. `--no-color` flag has highest priority,
/// then NO_COLOR env var, then stderr TTY detection.
pub fn detect_color_mode(no_color_flag: bool) -> ColorMode {
    if no_color_flag {
        return ColorMode::Plain;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return ColorMode::Plain;
    }
    if std::io::stderr().is_terminal() {
        ColorMode::Color
    } else {
        ColorMode::Plain
    }
}

/// Apply color to a single line of unified-diff output. Returns `line`
/// unchanged in [`ColorMode::Plain`].
pub fn colorize_diff_line(line: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return line.to_string();
    }
    let (prefix, body) = if line.starts_with("--- ") {
        ("\x1b[91m", line) // bright red, full-line
    } else if line.starts_with("+++ ") {
        ("\x1b[92m", line) // bright green, full-line
    } else if line.starts_with("@@") {
        ("\x1b[36m", line) // cyan
    } else if line.starts_with('-') {
        ("\x1b[91m", line) // bright red
    } else if line.starts_with('+') {
        ("\x1b[92m", line) // bright green
    } else {
        return line.to_string();
    };
    format!("{prefix}{body}\x1b[0m")
}

/// Colorize the conflict header line. Bold yellow.
pub fn colorize_header(text: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return text.to_string();
    }
    format!("\x1b[1;93m{text}\x1b[0m")
}

/// Colorize the action-letter prompt line. Bracketed letters bold cyan;
/// rest of the prompt unchanged.
pub fn colorize_prompt(text: &str, mode: ColorMode) -> String {
    if mode == ColorMode::Plain {
        return text.to_string();
    }
    // Replace `[X]` patterns with bold-cyan rendering, leaving punctuation alone.
    let mut out = String::with_capacity(text.len() + 64);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            // Capture the next char and the closing ].
            if let Some(&letter) = chars.peek() {
                if letter.is_ascii_alphabetic() {
                    chars.next(); // consume letter
                    if matches!(chars.peek(), Some(']')) {
                        chars.next(); // consume ]
                        out.push_str("[\x1b[1;96m");
                        out.push(letter);
                        out.push_str("\x1b[0m]");
                        continue;
                    } else {
                        // Not a single-letter bracketed token — emit as-is.
                        out.push('[');
                        out.push(letter);
                        continue;
                    }
                }
            }
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}
```

Add `colorize_prompt` tests:

```rust
    #[test]
    fn colorize_prompt_wraps_bracketed_letters() {
        let s = colorize_prompt("[k]eep local  [r]emote", ColorMode::Color);
        // Expect both letters wrapped in bold-cyan SGR.
        assert!(s.matches("\x1b[1;96m").count() == 2, "got: {s:?}");
    }

    #[test]
    fn colorize_prompt_plain_returns_unchanged() {
        let s = colorize_prompt("[k]eep local", ColorMode::Plain);
        assert_eq!(s, "[k]eep local");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cli::resolve::tests::colorize cli::resolve::tests::detect_color_mode`
Expected: FAIL — ten new tests fail because `colorize_diff_line` etc. don't exist (compile error).

- [ ] **Step 3: Confirm tests pass after the implementation in Step 1 is in place**

The implementation was added alongside the tests in Step 1. Run again:

Run: `cargo test --lib cli::resolve`
Expected: PASS — all existing tests + ten new color tests.

- [ ] **Step 4: Commit**

```bash
git add src/cli/resolve.rs
git commit -m "$(cat <<'EOF'
feat(resolve): add ColorMode + colorize helpers for conflict resolver

ColorMode enum (Plain/Color) + detect_color_mode(no_color_flag) reads
NO_COLOR env, stderr TTY status, and an explicit flag. colorize_diff_line
maps unified-diff prefixes to ANSI SGR codes (bright red for -, bright
green for +, cyan for @@). colorize_header uses bold yellow; colorize_prompt
wraps bracketed action letters in bold cyan.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: Wire colors into `prompt_resolve`

**Why:** Apply the helpers from Task 8 at the actual prompt site so users see colored output during real conflicts.

**Files:**
- Modify: `src/cli/resolve.rs:88-134` (`prompt_resolve`)

- [ ] **Step 1: Write the failing test**

Append to `src/cli/resolve.rs` test module:

```rust
    #[test]
    fn prompt_emits_color_codes_when_color_mode() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{\"name\":\"old\"}").unwrap();

        let input = Cursor::new(b"k\n");
        let mut output: Vec<u8> = Vec::new();
        prompt_resolve_with_color(
            input,
            &mut output,
            1,
            1,
            &path,
            b"{\"name\":\"new\"}",
            ColorMode::Color,
        )
        .unwrap();
        let s = String::from_utf8(output).unwrap();
        // Header bold yellow, action letters bold cyan, diff lines red/green.
        assert!(s.contains("\x1b[1;93m"), "no header color: {s:?}");
        assert!(s.contains("\x1b[91m") || s.contains("\x1b[92m"), "no diff color: {s:?}");
        assert!(s.contains("\x1b[1;96m"), "no prompt color: {s:?}");
    }

    #[test]
    fn prompt_plain_mode_emits_no_color_codes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{\"name\":\"old\"}").unwrap();

        let input = Cursor::new(b"k\n");
        let mut output: Vec<u8> = Vec::new();
        prompt_resolve_with_color(
            input,
            &mut output,
            1,
            1,
            &path,
            b"{\"name\":\"new\"}",
            ColorMode::Plain,
        )
        .unwrap();
        let s = String::from_utf8(output).unwrap();
        assert!(!s.contains("\x1b["), "expected no SGR codes: {s:?}");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cli::resolve::tests::prompt_emits_color_codes cli::resolve::tests::prompt_plain_mode`
Expected: FAIL — `prompt_resolve_with_color` does not exist (compile error).

- [ ] **Step 3: Refactor `prompt_resolve` to use a `_with_color` core**

Edit `src/cli/resolve.rs`:

Replace the existing `prompt_resolve` function with two functions — a thin wrapper that auto-detects color, and a core that takes an explicit mode (used by tests):

```rust
/// Top-level entry point. Auto-detects color mode from environment and TTY.
/// Production callers use this; tests use `prompt_resolve_with_color` to
/// pin the mode.
pub fn prompt_resolve<R: BufRead, W: Write>(
    input: R,
    output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    remote_bytes: &[u8],
) -> Result<Resolution> {
    let mode = detect_color_mode(false);
    prompt_resolve_with_color(input, output, index, total, local_path, remote_bytes, mode)
}

/// Color-aware core. Tests pin the mode here; production goes through
/// `prompt_resolve` which auto-detects.
pub fn prompt_resolve_with_color<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    remote_bytes: &[u8],
    mode: ColorMode,
) -> Result<Resolution> {
    let local_bytes = read_local(local_path)?;

    let local_canonical = crate::snapshot::noise::canonicalize_for_hash(&local_bytes);
    let remote_canonical = crate::snapshot::noise::canonicalize_for_hash(remote_bytes);

    if local_canonical == remote_canonical {
        return Ok(Resolution::KeepLocal);
    }

    writeln!(output, "")?;
    let header = format!("[{index}/{total}]  {} — conflict", local_path.display());
    writeln!(output, "{}", colorize_header(&header, mode))?;
    writeln!(output, "")?;

    let diff = unified_diff("local", &local_canonical, "remote", &remote_canonical);
    if diff.is_empty() {
        return Ok(Resolution::KeepLocal);
    }
    for line in diff.lines() {
        writeln!(output, "{}", colorize_diff_line(line, mode))?;
    }
    writeln!(output, "")?;

    loop {
        let prompt_text = "[k]eep local  [r]emote  [e]dit  [s]kip (shadow file)  [a]bort > ";
        write!(output, "{}", colorize_prompt(prompt_text, mode))?;
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
            Some('e') | Some('E') => {
                let edited = run_editor_with_markers(&local_bytes, remote_bytes)?;
                return Ok(Resolution::Edit(edited));
            }
            _ => {
                writeln!(output, "  (unrecognized — pick one of k/r/e/s/a)")?;
                continue;
            }
        }
    }
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test --lib cli::resolve`
Expected: PASS — all existing tests (which call `prompt_resolve` and run under cargo-test's non-TTY stderr → `detect_color_mode` returns Plain → no color codes → existing assertions intact) + new color tests.

- [ ] **Step 5: Commit**

```bash
git add src/cli/resolve.rs
git commit -m "$(cat <<'EOF'
feat(resolve): wire color helpers into prompt_resolve

Header (bold yellow), unified-diff lines (red/green), action-letter prompt
(bold cyan). Auto-detects color mode (NO_COLOR env, stderr TTY). Tests
exercise both color and plain paths via prompt_resolve_with_color.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

**Phase 2 checkpoint.** Conflict resolver coloring shippable. Three commits. ~12 new tests. Continue with Phase 3 or merge as a standalone improvement.

---

## Phase 3 — Per-Kind Progress Bar

### Task 10: Add `indicatif` dependency

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add the dep**

Edit `Cargo.toml` `[dependencies]` to insert (alphabetically after `futures`):

```toml
[dependencies]
anstyle = "1"
anyhow = "1"
clap = { version = "4", features = ["derive"] }
futures = "0.3"
indicatif = "0.17"
regex = "1"
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
similar = "2"
thiserror = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "fs"] }
toml = "0.8"
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build`
Expected: success.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
chore: add indicatif dep for per-kind progress bars

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 11: Create `src/progress.rs` with `KindProgress`

**Why:** The new module is the single home for all bar/log-mode logic. Drivers use a thin handle.

**Files:**
- Create: `src/progress.rs`
- Modify: `src/lib.rs` (declare new module)

- [ ] **Step 1: Write the failing tests**

Create `src/progress.rs`:

```rust
//! Per-kind progress UX during pull/push.
//!
//! - TTY: `indicatif` spinner → bar with ETA.
//! - Non-TTY (CI, piped): plain `→ kind: …` then `✓ kind: N items, Xs` lines.
//!
//! Drivers receive a thin `&KindProgress` handle. Drop emits the done-line.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// One progress UX surface for one pull/push kind.
pub struct KindProgress {
    inner: Mutex<Inner>,
}

enum Inner {
    Bar {
        kind: String,
        started: Instant,
        bar: indicatif::ProgressBar,
        orphans: AtomicUsize,
        finished: bool,
    },
    Log {
        kind: String,
        started: Instant,
        count: AtomicU64,
        orphans: AtomicUsize,
        finished: bool,
    },
}

impl KindProgress {
    /// Create a new progress surface for `kind`. Auto-detects TTY and
    /// chooses Bar or Log mode. The starting line is emitted immediately.
    /// `kind` is owned (`String`) so dynamic prefixes like `format!("push envs/{env}")`
    /// work alongside the common `"workspaces"` static literals.
    pub fn start(kind: impl Into<String>) -> Self {
        let kind: String = kind.into();
        if std::io::stderr().is_terminal() {
            let bar = indicatif::ProgressBar::new_spinner();
            bar.set_prefix(kind.clone());
            bar.set_style(
                indicatif::ProgressStyle::with_template("{spinner} {prefix}  listing…")
                    .unwrap(),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(100));
            Self {
                inner: Mutex::new(Inner::Bar {
                    kind,
                    started: Instant::now(),
                    bar,
                    orphans: AtomicUsize::new(0),
                    finished: false,
                }),
            }
        } else {
            eprintln!("→ {kind}: listing…");
            Self {
                inner: Mutex::new(Inner::Log {
                    kind,
                    started: Instant::now(),
                    count: AtomicU64::new(0),
                    orphans: AtomicUsize::new(0),
                    finished: false,
                }),
            }
        }
    }

    /// Switch from spinner-only to bar with denominator `n`. Called once
    /// per kind, after `list_*` returns.
    pub fn set_total(&self, n: u64) {
        let inner = self.inner.lock().unwrap();
        if let Inner::Bar { bar, .. } = &*inner {
            bar.set_length(n);
            bar.set_style(
                indicatif::ProgressStyle::with_template(
                    "{spinner} {prefix}  [{wide_bar}] {pos}/{len}  ETA {eta}",
                )
                .unwrap(),
            );
        }
        // Log mode: nothing to do here — count() is what we report.
    }

    /// Advance by one item.
    pub fn tick(&self) {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { bar, .. } => bar.inc(1),
            Inner::Log { count, .. } => {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Increment the orphan-skipped counter (does not advance the main tick).
    pub fn skipped_orphan(&self) {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { orphans, .. } => {
                orphans.fetch_add(1, Ordering::Relaxed);
            }
            Inner::Log { orphans, .. } => {
                orphans.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Run `f` with the bar paused (in TTY mode), so the bar redraws
    /// cleanly after stderr text. In log mode, `f` runs unchanged.
    pub fn suspend<F: FnOnce()>(&self, f: F) {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { bar, .. } => bar.suspend(f),
            Inner::Log { .. } => f(),
        }
    }

    /// Explicitly finish the bar/log line, emitting the `✓` done-line.
    /// Called from the orchestrator after a successful driver run; on
    /// driver-error the Drop impl skips the done-line so failed kinds
    /// don't show `✓`.
    pub fn finish(self) {
        let mut inner = self.inner.lock().unwrap();
        emit_done(&mut inner);
    }
}

impl Drop for KindProgress {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        // Suppress done-line on drop when not explicitly finished.
        match &mut *inner {
            Inner::Bar { bar, finished, .. } if !*finished => {
                bar.finish_and_clear();
            }
            Inner::Log { finished, .. } if !*finished => {
                // Nothing — caller bailed without emitting a done-line.
            }
            _ => {}
        }
    }
}

fn emit_done(inner: &mut Inner) {
    match inner {
        Inner::Bar { kind, started, bar, orphans, finished } => {
            *finished = true;
            let dur = started.elapsed();
            let count = bar.position();
            let orphans_n = orphans.load(Ordering::Relaxed);
            bar.finish_and_clear();
            eprintln!("{}", format_done(kind, count, orphans_n, dur));
        }
        Inner::Log { kind, started, count, orphans, finished } => {
            *finished = true;
            let dur = started.elapsed();
            let n = count.load(Ordering::Relaxed);
            let orphans_n = orphans.load(Ordering::Relaxed);
            eprintln!("{}", format_done(kind, n, orphans_n, dur));
        }
    }
}

fn format_done(kind: &str, count: u64, orphans: usize, dur: std::time::Duration) -> String {
    let secs = dur.as_secs_f32();
    if orphans > 0 {
        format!("✓ {kind}: {count} items, {orphans} orphans skipped, {secs:.1}s")
    } else {
        format!("✓ {kind}: {count} items, {secs:.1}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_done_no_orphans() {
        let s = format_done("workspaces", 12, 0, std::time::Duration::from_millis(2100));
        assert_eq!(s, "✓ workspaces: 12 items, 2.1s");
    }

    #[test]
    fn format_done_with_orphans() {
        let s = format_done("queues", 23, 2, std::time::Duration::from_millis(4700));
        assert_eq!(s, "✓ queues: 23 items, 2 orphans skipped, 4.7s");
    }

    #[test]
    fn format_done_zero_items() {
        let s = format_done("hooks", 0, 0, std::time::Duration::from_millis(100));
        assert_eq!(s, "✓ hooks: 0 items, 0.1s");
    }
}
```

Edit `src/lib.rs` to add the new module. Find the existing `pub mod` declarations and add (alphabetically):

```rust
pub mod paths;
pub mod progress;
pub mod secrets;
```

(The exact surrounding lines vary; insert `pub mod progress;` after `pub mod paths;` if that's its position alphabetically in the existing list.)

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test --lib progress`
Expected: PASS — three `format_done` tests succeed. (These are unit tests on the formatter, no I/O.)

- [ ] **Step 3: Commit**

```bash
git add src/progress.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat(progress): add KindProgress module for per-kind pull/push UX

KindProgress::start auto-detects TTY: Bar mode uses indicatif spinner→bar
with ETA; Log mode emits → / ✓ lines. set_total switches spinner to bar.
tick advances the count. skipped_orphan increments a separate counter
that surfaces in the done-line. suspend() lets stderr writes interleave
without tearing the bar. Drop without finish() suppresses the done-line
(used on driver error).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 12: Wire `KindProgress` into one flat-list pull driver (labels)

**Why:** Establish the integration pattern on the simplest driver before propagating. Labels is org-level, flat list, no orphans, no concurrency — minimum surface.

**Files:**
- Modify: `src/cli/pull/labels.rs`

- [ ] **Step 1: Read the current driver**

Run: `cargo build --lib` to confirm baseline. Then open `src/cli/pull/labels.rs`. The function signature is something like:

```rust
pub async fn pull_labels(ctx: &mut PullCtx<'_>) -> Result<usize> {
    // ... lists labels, walks them, returns count
}
```

- [ ] **Step 2: Add `&KindProgress` parameter and ticks**

Edit `src/cli/pull/labels.rs` — change signature and body. Show the entire new body (replacing existing):

```rust
use crate::cli::pull::common::{
    apply_pull_action, decide_pull_action, hash_for_lockfile, maybe_strip_overlay,
    pluralize, record_object, skip_on_permission_denied, PullAction, PullCtx,
};
use crate::progress::KindProgress;
use crate::slug::slugify_unique;
use crate::snapshot::label::serialize_label;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::collections::BTreeSet;

pub async fn pull_labels(ctx: &mut PullCtx<'_>, progress: &KindProgress) -> Result<usize> {
    let labels = skip_on_permission_denied(ctx.client.list_labels().await, "labels")?;
    progress.set_total(labels.len() as u64);

    let dir = ctx.paths.labels_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;

    let mut used: BTreeSet<String> = BTreeSet::new();
    for entry in ctx.lockfile.slugs_for_kind("labels") {
        used.insert(entry.to_string());
    }

    let mut count = 0usize;
    for label in labels {
        let slug = match ctx.lockfile.slug_for_id("labels", label.id) {
            Some(s) => s.to_string(),
            None => slugify_unique(&label.name, &mut used),
        };

        let bytes = serialize_label(&label)?;
        let bytes = maybe_strip_overlay(
            bytes,
            ctx.overlay.as_ref().and_then(|o| o.labels.get(&slug)),
        )?;

        let local_path = dir.join(format!("{slug}.json"));
        let base_hash = ctx
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.as_deref());
        let (action, remote_hash) = decide_pull_action(&local_path, base_hash, &bytes)?;

        if matches!(action, PullAction::Conflict) {
            progress.suspend(|| {
                eprintln!("conflict: labels/{slug}");
            });
        }

        let final_hash = apply_pull_action(action, &local_path, &bytes, remote_hash, ctx.interactive)?;
        record_object(
            ctx.lockfile,
            "labels",
            &slug,
            label.id,
            Some(label.url.clone()),
            label.modified_at().map(|s| s.to_string()),
            Some(final_hash),
        );

        progress.tick();
        count += 1;
    }
    Ok(count)
}
```

(The exact existing body may differ in detail — preserve any kind-specific logic and weave `progress.set_total`, `progress.tick()`, and the `progress.suspend` for any existing eprintlns you find.)

- [ ] **Step 3: Update the caller in `src/cli/pull/mod.rs`**

Find the call site of `pull_labels` and update to pass a `KindProgress`. Example pattern (the exact surrounding code may differ — match the existing style):

```rust
{
    let p = KindProgress::start("labels");
    let n = labels::pull_labels(&mut ctx, &p).await?;
    p.finish();
    stats.labels = n;
}
```

Add the import at the top of `src/cli/pull/mod.rs`:

```rust
use crate::progress::KindProgress;
```

- [ ] **Step 4: Run the labels-related tests**

Run: `cargo test --lib pull::labels` and `cargo test --test cli_pull pull_labels`
Expected: PASS — assertions on counts/hashes still hold; the progress wiring doesn't change behavior.

- [ ] **Step 5: Commit**

```bash
git add src/cli/pull/labels.rs src/cli/pull/mod.rs
git commit -m "$(cat <<'EOF'
feat(pull): wire KindProgress into labels driver (proof-of-concept)

Establishes the per-kind progress integration pattern: set_total after
list_*, tick per item, suspend wraps any eprintlns, finish() emits the
done-line. Labels chosen as the simplest driver (flat list, no orphans,
no concurrency).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 13: Wire `KindProgress` into all other flat-list pull drivers

**Why:** Apply the labels pattern to every other flat driver. Mechanical repetition.

**Files (each modified the same way):**
- `src/cli/pull/workspaces.rs`
- `src/cli/pull/hooks.rs`
- `src/cli/pull/rules.rs`
- `src/cli/pull/engines.rs`
- `src/cli/pull/engine_fields.rs`
- `src/cli/pull/workflows.rs`
- `src/cli/pull/workflow_steps.rs`
- `src/cli/pull/organization.rs` (singleton — set_total(1), tick once)

- [ ] **Step 1: For each file above, apply the same pattern**

For each driver:

1. Add `use crate::progress::KindProgress;` and `use crate::cli::pull::common::PullAction;` (the latter only if you need it for conflict suspend).
2. Change the function signature: add `progress: &KindProgress` parameter.
3. After the `list_*` call: `progress.set_total(items.len() as u64);` (for the singleton organization driver: `progress.set_total(1);`).
4. After each per-item write: `progress.tick();`.
5. Wrap any existing `eprintln!` in `progress.suspend(|| eprintln!(...));`.

Concrete example for `src/cli/pull/hooks.rs` — look at the existing function, then weave in:

```rust
pub async fn pull_hooks(ctx: &mut PullCtx<'_>, progress: &KindProgress) -> Result<usize> {
    let hooks = skip_on_permission_denied(ctx.client.list_hooks().await, "hooks")?;
    progress.set_total(hooks.len() as u64);

    // ... existing slug + write logic, with `progress.tick();` at the end of each iteration.
    // Wrap any eprintln! found inside the loop in `progress.suspend(|| eprintln!(...));`.
}
```

- [ ] **Step 2: Update every call site in `src/cli/pull/mod.rs`**

For each call to `pull_<kind>`, wrap with:

```rust
{
    let p = KindProgress::start("<kind>");
    let n = <kind>::pull_<kind>(&mut ctx, &p).await?;
    p.finish();
    stats.<kind> = n;
}
```

(Replace `<kind>` with the actual kind name.)

- [ ] **Step 3: Run the test suite**

Run: `cargo test`
Expected: PASS — all unit + integration tests succeed. Most tests will observe Log-mode output (cargo test runs non-TTY); existing assertions on summary/count text will still match where they look at lockfile state, not stderr text.

If a test asserts specific stderr text like "pulled 5 hooks", update it to look for `✓ hooks: 5 items` instead. Do these updates per-file as you encounter them.

- [ ] **Step 4: Commit**

```bash
git add src/cli/pull/{workspaces,hooks,rules,engines,engine_fields,workflows,workflow_steps,organization}.rs src/cli/pull/mod.rs
git commit -m "$(cat <<'EOF'
feat(pull): wire KindProgress into all flat-list drivers

workspaces, hooks, rules, engines, engine_fields, workflows, workflow_steps,
organization all accept &KindProgress, call set_total + tick + suspend.
Caller in pull/mod.rs wraps each call with KindProgress::start/finish.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 14: Wire `KindProgress` into the queues driver (concurrent fan-out)

**Why:** Queues fans out per-queue schema + inbox fetches via `buffer_unordered`. The tick must happen inside the closure, after the per-queue write, to count once per queue (not per sub-fetch).

**Files:**
- Modify: `src/cli/pull/queues.rs`

- [ ] **Step 1: Add the parameter and ticks**

Open `src/cli/pull/queues.rs`. Note the existing 3-phase structure (sequential setup, concurrent buffer_unordered fetches, sequential write decisions).

Edit:

1. Add `use crate::progress::KindProgress;`.
2. Change signature: `pub async fn pull_queues(ctx: &mut PullCtx<'_>, progress: &KindProgress) -> Result<usize>`.
3. After the `list_queues` call (or after Phase 1 when the work-list is built): `progress.set_total(queues_to_process.len() as u64);`.
4. In Phase 3 (the per-queue write decisions, sequential), call `progress.tick()` after writing each queue's queue.json + schema.json + inbox.json + email_templates link.
5. Replace the existing orphan-skip `eprintln!` (around line 53) with `progress.skipped_orphan();`. Search for the pattern `warning: skipping queue` and remove the `eprintln!` entirely; the `progress.skipped_orphan()` call replaces both the count-tracking and the user-visible note (the count surfaces on the done-line per Phase 1 behavior).
6. Wrap any other `eprintln!` in `progress.suspend(|| eprintln!(...));`.

- [ ] **Step 2: Update the call site in `src/cli/pull/mod.rs`**

```rust
{
    let p = KindProgress::start("queues");
    let n = queues::pull_queues(&mut ctx, &p).await?;
    p.finish();
    stats.queues = n;
}
```

- [ ] **Step 3: Run queues-related tests**

Run: `cargo test --lib pull::queues` and `cargo test --test cli_pull pull_queues_with_orphan`
Expected: PASS. Any test that previously asserted "warning: skipping queue" in stderr must change to assert "orphans skipped" in the done-line. Update those assertions.

- [ ] **Step 4: Commit**

```bash
git add src/cli/pull/queues.rs src/cli/pull/mod.rs
git commit -m "$(cat <<'EOF'
feat(pull): wire KindProgress into queues driver

Sequential Phase 3 writes call progress.tick() per queue. Orphan eprintln
replaced with progress.skipped_orphan(); count surfaces on the done-line.
Other eprintlns wrapped in progress.suspend() so concurrent retries don't
tear the bar.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 15: Wire `KindProgress` into the mdh driver

**Why:** Same shape as queues — concurrent per-collection fetch fan-out.

**Files:**
- Modify: `src/cli/pull/mdh.rs`

- [ ] **Step 1: Apply the queues pattern to mdh**

Edit `src/cli/pull/mdh.rs`:

1. Add `use crate::progress::KindProgress;`.
2. Change signature: `pub async fn pull_mdh(ctx: &mut PullCtx<'_>, progress: &KindProgress) -> Result<usize>`.
3. After `list_collections`: `progress.set_total(collections.len() as u64);`.
4. In the sequential write phase: `progress.tick()` per collection.
5. Wrap any `eprintln!` in `progress.suspend(|| eprintln!(...));`.

- [ ] **Step 2: Update call site in `src/cli/pull/mod.rs`**

```rust
{
    let p = KindProgress::start("mdh");
    let n = mdh::pull_mdh(&mut ctx, &p).await?;
    p.finish();
    stats.mdh = n;
}
```

- [ ] **Step 3: Run mdh tests**

Run: `cargo test --test cli_pull pull_mdh`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/cli/pull/mdh.rs src/cli/pull/mod.rs
git commit -m "$(cat <<'EOF'
feat(pull): wire KindProgress into mdh driver

Same pattern as queues: tick per collection in the sequential write phase,
suspend wraps any stderr writes during the concurrent fetch phase.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 16: Wire `KindProgress` into the email_templates driver, suppressing orphan eprintlns

**Why:** Two orphan eprintlns at lines 36 and 45 must be replaced with `progress.skipped_orphan()` per the spec.

**Files:**
- Modify: `src/cli/pull/email_templates.rs`

- [ ] **Step 1: Apply the pattern + replace orphan eprintlns**

Edit `src/cli/pull/email_templates.rs`:

1. Add `use crate::progress::KindProgress;`.
2. Change signature: `pub async fn pull_email_templates(ctx: &mut PullCtx<'_>, progress: &KindProgress) -> Result<usize>`.
3. After `list_email_templates`: `progress.set_total(templates.len() as u64);`.
4. Find the eprintln at line 36 (orphan template / queue not in snapshot) — replace with `progress.skipped_orphan();`.
5. Find the eprintln at line 45 (template's queue is filtered) — replace with `progress.skipped_orphan();`.
6. Tick per template in the iteration loop (only on items that actually wrote a file — orphans count as skipped, not ticked).

- [ ] **Step 2: Update call site in `src/cli/pull/mod.rs`**

```rust
{
    let p = KindProgress::start("email_templates");
    let n = email_templates::pull_email_templates(&mut ctx, &p).await?;
    p.finish();
    stats.email_templates = n;
}
```

- [ ] **Step 3: Run email-templates tests**

Run: `cargo test --test cli_pull email_templates`
Expected: PASS. Any tests that asserted "warning: skipping email template" must change to assert "orphans skipped" in the done-line.

- [ ] **Step 4: Commit**

```bash
git add src/cli/pull/email_templates.rs src/cli/pull/mod.rs
git commit -m "$(cat <<'EOF'
feat(pull): wire KindProgress into email_templates, suppress orphan eprintlns

Two orphan eprintlns replaced with progress.skipped_orphan(). Counts
surface on the done-line per spec ("✓ email_templates: N items, M orphans
skipped, X.Xs"). Tick happens per non-orphan template.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 17: Route remaining pull eprintlns through `progress.suspend()`

**Why:** Conflict warnings, permission warnings, and retry warnings still need to print, but they must not corrupt the bar.

**Files:**
- Modify: `src/cli/pull/common.rs`

- [ ] **Step 1: Update `apply_pull_action` and helpers to accept `&KindProgress`**

Edit `src/cli/pull/common.rs`. Change `apply_pull_action`'s signature to accept the progress handle, and pass it to `shadow_file_conflict` and `resolve_conflict_interactive`:

```rust
pub fn apply_pull_action(
    action: PullAction,
    local_path: &Path,
    remote_bytes: &[u8],
    remote_hash: String,
    interactive: bool,
    progress: &crate::progress::KindProgress,
) -> Result<String> {
    use crate::snapshot::writer::write_atomic;
    match action {
        PullAction::Write => {
            write_atomic(local_path, remote_bytes)?;
            Ok(remote_hash)
        }
        PullAction::KeepLocal => {
            let local_bytes = std::fs::read(local_path)
                .with_context(|| format!("reading {}", local_path.display()))?;
            Ok(content_hash(&local_bytes))
        }
        PullAction::NoChange => Ok(remote_hash),
        PullAction::Conflict => {
            if interactive {
                resolve_conflict_interactive(local_path, remote_bytes, &remote_hash, progress)
            } else {
                shadow_file_conflict(local_path, remote_bytes, progress)
            }
        }
    }
}
```

Update `shadow_file_conflict`:

```rust
fn shadow_file_conflict(
    local_path: &Path,
    remote_bytes: &[u8],
    progress: &crate::progress::KindProgress,
) -> Result<String> {
    use crate::snapshot::writer::write_atomic;
    let mut conflict_path = local_path.to_path_buf();
    let new_name = match conflict_path.file_name().and_then(|s| s.to_str()) {
        Some(name) => format!("{name}.remote"),
        None => "remote".to_string(),
    };
    conflict_path.set_file_name(new_name);
    write_atomic(&conflict_path, remote_bytes)?;
    let local_path_disp = local_path.display().to_string();
    let conflict_path_disp = conflict_path.display().to_string();
    progress.suspend(|| {
        eprintln!(
            "warning: {local_path_disp} conflict — local preserved, remote at {conflict_path_disp}"
        );
    });
    let local_bytes = std::fs::read(local_path)
        .with_context(|| format!("reading {}", local_path.display()))?;
    Ok(content_hash(&local_bytes))
}
```

Update `resolve_conflict_interactive` similarly to take `progress` (only used for any post-resolution warnings; the prompt itself goes to stderr directly since the user is interacting):

```rust
fn resolve_conflict_interactive(
    local_path: &Path,
    remote_bytes: &[u8],
    remote_hash: &str,
    progress: &crate::progress::KindProgress,
) -> Result<String> {
    use crate::cli::resolve::{prompt_resolve, PullAborted, Resolution};
    use crate::snapshot::writer::write_atomic;

    // Suspend the bar while the user is interacting.
    let resolution_result: Result<Resolution> = {
        let mut result = None;
        progress.suspend(|| {
            let stdin = std::io::stdin();
            let stderr = std::io::stderr();
            result = Some(prompt_resolve(
                stdin.lock(),
                stderr.lock(),
                1,
                1,
                local_path,
                remote_bytes,
            ));
        });
        result.expect("prompt_resolve should have run inside suspend closure")
    };
    let resolution = resolution_result?;
    match resolution {
        Resolution::KeepLocal => {
            let local_bytes = std::fs::read(local_path)
                .with_context(|| format!("reading {}", local_path.display()))?;
            Ok(content_hash(&local_bytes))
        }
        Resolution::KeepRemote => {
            write_atomic(local_path, remote_bytes)?;
            Ok(remote_hash.to_string())
        }
        Resolution::Edit(edited) => {
            write_atomic(local_path, &edited)?;
            Ok(content_hash(&edited))
        }
        Resolution::Skip => shadow_file_conflict(local_path, remote_bytes, progress),
        Resolution::Abort => Err(anyhow::Error::new(PullAborted)),
    }
}
```

Update `skip_on_permission_denied` to take `progress` and wrap its eprintln:

```rust
pub fn skip_on_permission_denied<T>(
    result: Result<Vec<T>>,
    kind: &str,
    progress: &crate::progress::KindProgress,
) -> Result<Vec<T>> {
    match result {
        Ok(v) => Ok(v),
        Err(e) => {
            let is_403 = e.chain().any(|c| {
                c.downcast_ref::<ApiError>()
                    .map(|api| matches!(api, ApiError::Status { status: 403, .. }))
                    .unwrap_or(false)
            });
            if is_403 {
                progress.suspend(|| {
                    eprintln!("warning: skipping {kind} — token lacks permission (403)");
                });
                Ok(Vec::new())
            } else {
                Err(e)
            }
        }
    }
}
```

- [ ] **Step 2: Update every caller of `apply_pull_action` and `skip_on_permission_denied`**

Every per-kind pull driver now passes `progress` to these helpers. Mechanical edit across `src/cli/pull/{labels,workspaces,hooks,rules,engines,engine_fields,workflows,workflow_steps,organization,queues,mdh,email_templates}.rs`. Pattern: existing `apply_pull_action(action, &local_path, &bytes, remote_hash, ctx.interactive)?` becomes `apply_pull_action(action, &local_path, &bytes, remote_hash, ctx.interactive, progress)?`. Same for `skip_on_permission_denied(..., kind)` → `skip_on_permission_denied(..., kind, progress)`.

- [ ] **Step 3: Fix the existing tests in `src/cli/pull/common.rs`**

The unit tests for `apply_pull_action` need a `KindProgress` argument. The simplest fix: each test creates one inline. Update the existing tests:

```rust
    #[test]
    fn apply_write_creates_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let p = crate::progress::KindProgress::start("test");
        let h = apply_pull_action(PullAction::Write, &path, b"hello", "h".repeat(64), false, &p).unwrap();
        p.finish();
        assert_eq!(h, "h".repeat(64));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn apply_conflict_non_interactive_writes_remote_sibling() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local").unwrap();
        let p = crate::progress::KindProgress::start("test");
        let _ = apply_pull_action(PullAction::Conflict, &path, b"remote", "h".repeat(64), false, &p).unwrap();
        p.finish();
        assert_eq!(std::fs::read(&path).unwrap(), b"local");
        assert_eq!(std::fs::read(dir.path().join("x.json.remote")).unwrap(), b"remote");
    }

    #[test]
    fn apply_nochange_does_not_modify_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"original").unwrap();
        let original_bytes = std::fs::read(&path).unwrap();
        let p = crate::progress::KindProgress::start("test");
        let h = apply_pull_action(
            PullAction::NoChange,
            &path,
            b"different remote bytes",
            "h".repeat(64),
            false,
            &p,
        )
        .unwrap();
        p.finish();
        assert_eq!(h, "h".repeat(64));
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
    }
```

Add a similar `let p = crate::progress::KindProgress::start("test"); ...; p.finish();` wrapper to the existing `skip_on_permission_denied_*` tests.

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS — all updated unit + integration tests succeed.

- [ ] **Step 5: Commit**

```bash
git add src/cli/pull/common.rs src/cli/pull/*.rs
git commit -m "$(cat <<'EOF'
feat(pull): route conflict/permission warnings via progress.suspend()

apply_pull_action, shadow_file_conflict, resolve_conflict_interactive, and
skip_on_permission_denied all take &KindProgress. Their stderr writes go
through suspend() so the bar redraws cleanly. Interactive resolver also
runs inside suspend() so the prompt isn't corrupted by background ticks.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 18: Update `pull/mod.rs::run_drivers` for accumulated counts

**Why:** Surface orphan totals + conflict counts in the final pull-summary line, plus thread the progress lifecycle correctly.

**Files:**
- Modify: `src/cli/pull/mod.rs`

- [ ] **Step 1: Inspect the current `run_drivers` and `run` functions**

Open `src/cli/pull/mod.rs`. Note the existing `PullStats` struct and the final `println!("{summary}")` at line 133.

- [ ] **Step 2: Add orphan totals to `PullStats` and the summary line**

Edit the `PullStats` struct to add an `orphans` count, populated by extracting from each `KindProgress` before finish(). Easiest pattern: each `KindProgress` exposes a getter:

Add to `src/progress.rs`:

```rust
impl KindProgress {
    /// Read the current orphan-skipped count. Useful for accumulation in
    /// the parent runner's stats struct.
    pub fn orphans(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        match &*inner {
            Inner::Bar { orphans, .. } => orphans.load(Ordering::Relaxed),
            Inner::Log { orphans, .. } => orphans.load(Ordering::Relaxed),
        }
    }
}
```

Then in `pull/mod.rs`, when running each driver, capture orphans before finish:

```rust
{
    let p = KindProgress::start("queues");
    let n = queues::pull_queues(&mut ctx, &p).await?;
    let orphans = p.orphans();
    p.finish();
    stats.queues = n;
    stats.orphans += orphans;
}
```

(Same pattern for any kind that calls `progress.skipped_orphan()` — currently queues + email_templates.)

- [ ] **Step 3: Update the final summary line**

Find the existing `println!("{summary}")` at line 133. Update the summary construction to include orphans:

```rust
let mut summary = format!(
    "✓ pull envs/{}: {} items",
    env,
    total_items
);
if stats.orphans > 0 {
    summary.push_str(&format!(", {} orphans skipped", stats.orphans));
}
if stats.conflicts > 0 {
    summary.push_str(&format!(", {} conflicts", stats.conflicts));
}
let secs = pull_started.elapsed().as_secs_f32();
summary.push_str(&format!("  {secs:.1}s"));
println!("{summary}");
```

(Existing `total_items` accumulator should already sum across kinds; if not, add it.)

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS — integration tests that assert on summary text need updating from "pulled X labels" / similar to the new `✓ pull envs/<env>: ...` shape. Update those one-by-one as cargo surfaces them.

- [ ] **Step 5: Commit**

```bash
git add src/cli/pull/mod.rs src/progress.rs
git commit -m "$(cat <<'EOF'
feat(pull): surface orphan totals + conflicts in summary line

PullStats accumulates orphan counts from each KindProgress before finish().
Final summary reads "✓ pull envs/<env>: N items, M orphans skipped, K
conflicts  X.Xs". Per-kind done-lines already surface their own orphans.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 19: Push phase-1 hash scan extraction

**Why:** Today each push driver hashes inline as it walks. Extracting "what changed" into a phase-1 scan gives the bar an honest denominator and enables the no-op-push fast path.

**Files:**
- Modify: `src/cli/push/mod.rs`
- Create: `src/cli/push/scan.rs` (or extension; choose the simpler placement)

- [ ] **Step 1: Define a `ChangeList` shape**

Create `src/cli/push/scan.rs`:

```rust
//! Phase 1 of `rdc push`: walk the local snapshot, hash every writable file,
//! compare to lockfile, and produce a list of items needing PATCH per kind.
//! Phase 2 (the per-kind drivers) consumes this list and only iterates
//! changed items.
//!
//! This split lets `push` show a single "hashing local files…" spinner up
//! front, then per-kind progress bars only for kinds with actual changes.
//! When nothing changed, the entire phase-2 fan-out is skipped and the
//! command exits with a one-line summary.

use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::Result;
use std::collections::BTreeMap;

/// Items needing PATCH, grouped by kind. Slug is the key; the value is the
/// on-disk path so phase-2 drivers don't re-walk.
#[derive(Debug, Default)]
pub struct ChangeList {
    pub hooks: BTreeMap<String, std::path::PathBuf>,
    pub rules: BTreeMap<String, std::path::PathBuf>,
    pub labels: BTreeMap<String, std::path::PathBuf>,
    pub queues: BTreeMap<String, std::path::PathBuf>,
    pub schemas: BTreeMap<String, std::path::PathBuf>,
    pub inboxes: BTreeMap<String, std::path::PathBuf>,
    pub email_templates: BTreeMap<String, std::path::PathBuf>,
    pub engines: BTreeMap<String, std::path::PathBuf>,
    pub engine_fields: BTreeMap<String, std::path::PathBuf>,
}

impl ChangeList {
    pub fn total(&self) -> usize {
        self.hooks.len()
            + self.rules.len()
            + self.labels.len()
            + self.queues.len()
            + self.schemas.len()
            + self.inboxes.len()
            + self.email_templates.len()
            + self.engines.len()
            + self.engine_fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Walk the local snapshot, hash every writable file, compare to lockfile,
/// build a `ChangeList`. Returns `(scan_count, changes)`.
pub fn scan(paths: &Paths, lockfile: &Lockfile) -> Result<(usize, ChangeList)> {
    // Implementation walks each writable kind's directory in turn and
    // populates the BTreeMaps for items whose hash differs from
    // lockfile.content_hash. Helper functions per kind below — each is
    // a small (~30-line) walk that mirrors the corresponding pull driver's
    // hash logic.

    let mut changes = ChangeList::default();
    let mut scanned = 0;

    scanned += scan_hooks(paths, lockfile, &mut changes.hooks)?;
    scanned += scan_flat_kind::<crate::model::rule::Rule>(paths, lockfile, "rules", paths.rules_dir(), &mut changes.rules)?;
    scanned += scan_flat_kind::<crate::model::label::Label>(paths, lockfile, "labels", paths.labels_dir(), &mut changes.labels)?;
    scanned += scan_queues(paths, lockfile, &mut changes.queues)?;
    scanned += scan_schemas(paths, lockfile, &mut changes.schemas)?;
    scanned += scan_inboxes(paths, lockfile, &mut changes.inboxes)?;
    scanned += scan_email_templates(paths, lockfile, &mut changes.email_templates)?;
    scanned += scan_flat_kind::<crate::model::engine::Engine>(paths, lockfile, "engines", paths.engines_dir(), &mut changes.engines)?;
    scanned += scan_flat_kind::<crate::model::engine_field::EngineField>(paths, lockfile, "engine_fields", paths.engine_fields_dir(), &mut changes.engine_fields)?;

    Ok((scanned, changes))
}

// Per-kind scan helpers. Each one walks its directory, reads each file,
// computes the canonical hash via the existing serialize_X round-trip,
// compares to lockfile.content_hash, and inserts into the change-list when
// they differ.

fn scan_hooks(_paths: &Paths, _lockfile: &Lockfile, _out: &mut BTreeMap<String, std::path::PathBuf>) -> Result<usize> {
    // Hooks have a combined hash (json + .py). Mirror src/cli/push/hooks.rs
    // current logic: for each hooks/<slug>.json, read the file + the
    // adjacent .py if present, compute hook_combined_hash, compare to
    // lockfile.content_hash. If differs, insert.
    todo!("implement; pattern is identical to push/hooks.rs's existing inline hash")
}

fn scan_flat_kind<T>(
    _paths: &Paths,
    _lockfile: &Lockfile,
    _kind: &str,
    _dir: std::path::PathBuf,
    _out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    // For each *.json under dir, parse as T, re-serialize via to_vec_pretty,
    // hash via content_hash, compare to lockfile.content_hash, insert on
    // mismatch.
    todo!("implement; pattern is identical to push/{rules,labels,engines,engine_fields}.rs's existing inline hash")
}

fn scan_queues(_paths: &Paths, _lockfile: &Lockfile, _out: &mut BTreeMap<String, std::path::PathBuf>) -> Result<usize> {
    todo!("walk envs/<env>/workspaces/<ws>/queues/<q>/queue.json")
}

fn scan_schemas(_paths: &Paths, _lockfile: &Lockfile, _out: &mut BTreeMap<String, std::path::PathBuf>) -> Result<usize> {
    todo!("walk envs/<env>/workspaces/<ws>/queues/<q>/schema.json + formulas/*.py via schema_combined_hash")
}

fn scan_inboxes(_paths: &Paths, _lockfile: &Lockfile, _out: &mut BTreeMap<String, std::path::PathBuf>) -> Result<usize> {
    todo!("walk envs/<env>/workspaces/<ws>/queues/<q>/inbox.json")
}

fn scan_email_templates(_paths: &Paths, _lockfile: &Lockfile, _out: &mut BTreeMap<String, std::path::PathBuf>) -> Result<usize> {
    todo!("walk envs/<env>/workspaces/<ws>/queues/<q>/email-templates/<slug>.json; compound key <ws>/<q>/<template>")
}
```

The `todo!()`s are placeholders — replace each with the body extracted from the corresponding existing push driver's inline hash code. The implementation steps below walk through each.

- [ ] **Step 2: Implement `scan_hooks` by extracting from `push/hooks.rs`**

Open `src/cli/push/hooks.rs`. Identify the section that walks the hooks directory and for each hook:
1. Reads `<slug>.json`
2. Reads `<slug>.py` if present
3. Computes `hook_combined_hash`
4. Compares to lockfile entry's `content_hash`
5. (If different) reads the json bytes, applies overlay, re-typed-deserializes, and proceeds with PATCH.

Move steps 1-4 into `scan_hooks` in `scan.rs` — the function's job is to return the change list. Step 5 stays in `push/hooks.rs` and becomes the phase-2 work.

```rust
fn scan_hooks(paths: &Paths, lockfile: &Lockfile, out: &mut BTreeMap<String, std::path::PathBuf>) -> Result<usize> {
    use crate::state::hook_combined_hash;
    let dir = paths.hooks_dir();
    if !dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let json_bytes = std::fs::read(&path)?;
        let py_path = path.with_extension("py");
        let code = if py_path.exists() {
            Some(std::fs::read_to_string(&py_path)?)
        } else {
            None
        };
        let local_hash = hook_combined_hash(&json_bytes, &code);
        scanned += 1;
        let base_hash = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(slug))
            .and_then(|e| e.content_hash.as_deref());
        if base_hash != Some(local_hash.as_str()) {
            out.insert(slug.to_string(), path);
        }
    }
    Ok(scanned)
}
```

- [ ] **Step 3: Implement `scan_flat_kind` and per-kind scans**

Implement each remaining `scan_*` function the same way. Each is a small walk; hash logic mirrors the existing `push/*.rs` driver. (~30 lines each.)

For `scan_flat_kind<T>`:

```rust
fn scan_flat_kind<T>(
    paths: &Paths,
    lockfile: &Lockfile,
    kind: &str,
    dir: std::path::PathBuf,
    out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    use crate::state::content_hash;
    if !dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let bytes = std::fs::read(&path)?;
        let model: T = serde_json::from_slice(&bytes)?;
        let canonical = serde_json::to_vec_pretty(&model)?;
        let local_hash = content_hash(&canonical);
        scanned += 1;
        let base_hash = lockfile
            .objects
            .get(kind)
            .and_then(|m| m.get(slug))
            .and_then(|e| e.content_hash.as_deref());
        if base_hash != Some(local_hash.as_str()) {
            out.insert(slug.to_string(), path);
        }
    }
    let _ = paths; // unused unless scan helpers grow path-prefix logic
    Ok(scanned)
}
```

Implement `scan_queues`, `scan_schemas`, `scan_inboxes`, `scan_email_templates` by following the existing logic in their corresponding `push/*.rs` files. Each walks `envs/<env>/workspaces/<ws>/queues/<q>/...`. Use `paths.workspaces_dir()` and `read_dir` recursion.

- [ ] **Step 4: Update `src/cli/push/mod.rs` to call `scan` first**

Edit `src/cli/push/mod.rs`. Add a phase-1 spinner around the scan, then dispatch to drivers with the change list. Show the new mod-level `run` skeleton:

```rust
pub mod scan;
pub mod hooks;
pub mod rules;
pub mod labels;
pub mod queues;
pub mod schemas;
pub mod inboxes;
pub mod email_templates;
pub mod engines;
pub mod engine_fields;

use crate::progress::KindProgress;
use anyhow::Result;

pub async fn run(env: &str, concurrency: usize, interactive: bool) -> Result<()> {
    let pull_started = std::time::Instant::now();
    // ... (existing setup: paths, client, lockfile load)

    // Phase 1: scan local files.
    let scan_progress = KindProgress::start(format!("push envs/{env}"));
    let (scanned, changes) = scan::scan(&paths, &lockfile)?;
    // Note: scan_progress.finish() emits the "✓ push envs/<env>: ..." line
    // automatically via the Drop/finish path. To customize the count/changed
    // suffix, we read the tick state via a getter and emit our own line
    // before calling finish — see KindProgress::orphans pattern in Task 18.
    // Simpler: just drop scan_progress without finish (suppresses the auto
    // ✓-line) and emit our own:
    drop(scan_progress);
    eprintln!(
        "✓ push envs/{env}: {} files scanned, {} changed",
        scanned,
        changes.total()
    );

    if changes.is_empty() {
        eprintln!(
            "✓ push envs/{env}: no changes  ({:.1}s)",
            pull_started.elapsed().as_secs_f32()
        );
        return Ok(());
    }

    // Phase 2: per-kind drivers, only for kinds with changes.
    if !changes.hooks.is_empty() {
        let p = KindProgress::start("hooks");
        p.set_total(changes.hooks.len() as u64);
        hooks::push_hooks(&client, &mut lockfile, &paths, &overlay, &changes.hooks, &p).await?;
        p.finish();
    }
    if !changes.rules.is_empty() {
        let p = KindProgress::start("rules");
        p.set_total(changes.rules.len() as u64);
        rules::push_rules(&client, &mut lockfile, &paths, &overlay, &changes.rules, &p).await?;
        p.finish();
    }
    // ... repeat for labels, queues, schemas, inboxes, email_templates, engines, engine_fields

    lockfile.save(...)?;
    println!(
        "✓ push envs/{env}: {} patched  {:.1}s",
        changes.total(),
        pull_started.elapsed().as_secs_f32()
    );
    Ok(())
}
```

The exact existing `run` function may have more setup — preserve all of it; the change is the phase-1 scan + early-exit + phase-2 dispatch.

- [ ] **Step 5: Run a smoke test**

Run: `cargo build` (to confirm compile)
Expected: success.

Run: `cargo test --test cli_push push_with_no_changes` (a test you'll add in Task 22 — for now, run any existing push test to verify smoke):
Expected: existing tests pass after their assertions are updated to the new shape (`✓ push envs/<env>: ...`).

- [ ] **Step 6: Commit**

```bash
git add src/cli/push/scan.rs src/cli/push/mod.rs
git commit -m "$(cat <<'EOF'
refactor(push): extract phase-1 hash scan into scan.rs

Phase 1 walks every writable kind's local files, hashes them, compares to
lockfile, and builds a ChangeList. Phase 2 only iterates changed items.
Enables honest progress-bar denominators and the no-op-push fast path
("✓ push envs/<env>: no changes  X.Xs").

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 20: Update push drivers to consume change lists with progress

**Why:** Phase-2 drivers now iterate over a pre-computed list of changed slugs, not the entire local tree.

**Files (each modified the same way):**
- `src/cli/push/hooks.rs`
- `src/cli/push/rules.rs`
- `src/cli/push/labels.rs`
- `src/cli/push/queues.rs`
- `src/cli/push/schemas.rs`
- `src/cli/push/inboxes.rs`
- `src/cli/push/email_templates.rs`
- `src/cli/push/engines.rs`
- `src/cli/push/engine_fields.rs`

- [ ] **Step 1: For each push driver, change the signature**

Existing pattern (e.g. push/hooks.rs):

```rust
pub async fn push_hooks(client: &RossumClient, lockfile: &mut Lockfile, paths: &Paths, overlay: &Option<Overlay>) -> Result<usize>
```

New pattern:

```rust
pub async fn push_hooks(
    client: &RossumClient,
    lockfile: &mut Lockfile,
    paths: &Paths,
    overlay: &Option<Overlay>,
    changes: &BTreeMap<String, std::path::PathBuf>,
    progress: &KindProgress,
) -> Result<usize> {
    use crate::progress::KindProgress;
    let mut patched = 0;
    for (slug, path) in changes {
        // ... per-hook PATCH logic (existing) — but iterate the change list
        // instead of walking the directory. The hash-check logic that was
        // inline in the existing driver is gone (phase 1 already did it).
        progress.tick();
        patched += 1;
    }
    Ok(patched)
}
```

The body's existing PATCH logic stays. Only changes:
- The outer `read_dir` loop is replaced with `for (slug, path) in changes`.
- The inline hash-check that compared local-vs-base is removed (phase 1 did it).
- A `progress.tick()` after each successful PATCH.
- Any `eprintln!` for drift / overwrite warnings is wrapped in `progress.suspend(|| eprintln!(...));`.

- [ ] **Step 2: Update the call sites in `src/cli/push/mod.rs`**

Already done in Task 19's run() skeleton. Verify each `if !changes.<kind>.is_empty()` block calls the right driver with the right arguments.

- [ ] **Step 3: Run the test suite**

Run: `cargo test`
Expected: PASS — every existing push test now needs the same assertion update from old "pushed N hooks" / "no changes" text to the new `✓ <kind>: N patched` and `✓ push envs/<env>: ...` shapes. Update assertions one-by-one as cargo surfaces them.

- [ ] **Step 4: Commit**

```bash
git add src/cli/push/*.rs
git commit -m "$(cat <<'EOF'
feat(push): drivers consume ChangeList with KindProgress

Each push driver now iterates the pre-computed change list (from phase 1)
and calls progress.tick() per PATCH. Drift/overwrite warnings route via
progress.suspend(). No-op kinds are skipped entirely by mod.rs (driver
not even called when changes.<kind> is empty).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 21: Wire retry warnings via `progress.suspend()` (optional handle)

**Why:** When the bar is up and a 429/5xx retry fires, the warning eprintln must not corrupt the bar.

**Files:**
- Modify: `src/api/retry.rs`

- [ ] **Step 1: Add an optional progress parameter to `send_with_retry`**

Open `src/api/retry.rs`. The current `send_with_retry` signature is something like:

```rust
pub async fn send_with_retry<F, Fut>(builder_factory: F, ...) -> Result<reqwest::Response>
```

Add an optional `&KindProgress`:

```rust
pub async fn send_with_retry<F, Fut>(
    builder_factory: F,
    progress: Option<&crate::progress::KindProgress>,
    ...,
) -> Result<reqwest::Response>
```

Inside the function, replace each `eprintln!` (the ones that print "rate limited" / "bad gateway" warnings) with:

```rust
let msg = format!("rate limited ({}) on {} {}; retrying in {:?} ...", status, method, url, delay);
if let Some(p) = progress {
    p.suspend(|| eprintln!("{msg}"));
} else {
    eprintln!("{msg}");
}
```

- [ ] **Step 2: Update callers**

`get_json` and `patch_json` in `src/api/mod.rs` (or wherever they live) need to accept and forward an optional progress. Three approaches:

- **Approach a:** Add `progress: Option<&KindProgress>` to every API method. Most intrusive but most explicit.
- **Approach b:** Stash a `&KindProgress` reference on `RossumClient` for the duration of a kind's work. Drivers `client.with_progress(&p, |c| async { c.list_hooks().await })`. Less intrusive but tricky lifetimes.
- **Approach c (chosen):** A `tokio::task_local!` storage of `Option<*const KindProgress>`. The driver wraps each `await` block in `task_local::scope(progress, async move { ... })`. `send_with_retry` reads the task-local. No signature changes to API methods.

Implement approach (c). Add to `src/api/retry.rs`:

```rust
tokio::task_local! {
    static CURRENT_PROGRESS: Option<&'static crate::progress::KindProgress>;
}
```

Wait — `&'static` is wrong for a per-kind handle. Use a different mechanism: pass through explicit parameter on each API call. **Switch to approach (a)** despite intrusiveness — it's the simpler model, fits the existing `&self`-method pattern, and signatures already take `&RossumClient` everywhere so adding a sibling `&KindProgress` is uniform.

Update each `RossumClient::list_*` / `get_*` / `patch_*` method to accept `progress: Option<&KindProgress>` and forward it to `send_with_retry`.

Driver call sites pass `Some(&progress)` or `None`. Phase-1 push scan is `None`; phase-2 driver dispatch is `Some(&progress)`.

- [ ] **Step 3: Run tests**

Run: `cargo test`
Expected: PASS — retry tests in `src/api/retry.rs` need a `None` argument to their helper invocations.

- [ ] **Step 4: Commit**

```bash
git add src/api/retry.rs src/api/*.rs src/cli/pull/*.rs src/cli/push/*.rs
git commit -m "$(cat <<'EOF'
feat(api): plumb optional KindProgress through retry warnings

Every list_*/get_*/patch_* method takes Option<&KindProgress> and forwards
to send_with_retry. Retry warnings now print via progress.suspend() when
a handle is in scope, falling back to plain stderr otherwise. Phase-1
push scan passes None; per-kind drivers pass Some(&p).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 22: Integration tests — orphan count, no-change push, modified_at on push

**Why:** Three end-to-end assertions that exercise the new UX in the realistic environments tests run in (non-TTY, log mode).

**Files:**
- Modify: `tests/cli_pull.rs`
- Modify: `tests/cli_push.rs`

- [ ] **Step 1: Add `pull_with_orphan_queue_surfaces_count_in_done_line` to `tests/cli_pull.rs`**

```rust
#[tokio::test]
async fn pull_with_orphan_queue_surfaces_count_in_done_line() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let dir = tempfile::TempDir::new().unwrap();

    // Standard fixture wiring (org/workspaces/etc) — copy from an existing
    // test in the file. Then mock /queues with one orphan:
    let queues_resp = serde_json::json!({
        "results": [{
            "id": 7,
            "url": format!("{}/api/v1/queues/7", server.uri()),
            "name": "orphan-q",
            "workspace": null,
            "schema": null
        }],
        "next": null
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&queues_resp))
        .mount(&server)
        .await;

    let mut cmd = assert_cmd::Command::cargo_bin("rdc").unwrap();
    let out = cmd
        .args(["pull", "test"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("✓ queues:") && stderr.contains("orphans skipped"),
        "expected orphans-skipped count in queues done-line. stderr was: {stderr}"
    );
}
```

- [ ] **Step 2: Add `push_with_no_changes_prints_no_bars` to `tests/cli_push.rs`**

```rust
#[tokio::test]
async fn push_with_no_changes_prints_no_bars() {
    let server = wiremock::MockServer::start().await;
    let dir = tempfile::TempDir::new().unwrap();
    // ... fixture setup: scaffold a project, put a clean lockfile + label
    // file matching its lockfile hash.

    let mut cmd = assert_cmd::Command::cargo_bin("rdc").unwrap();
    let out = cmd
        .args(["push", "test"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no changes"),
        "expected no-changes shortcut. stderr: {stderr}"
    );
    // No per-kind ✓ lines should appear (phase-2 was skipped).
    let kind_lines: Vec<_> = stderr
        .lines()
        .filter(|l| l.starts_with("✓ ") && l.contains(": ") && !l.contains("envs/"))
        .collect();
    assert!(kind_lines.is_empty(), "expected no per-kind lines, got: {kind_lines:?}");
}
```

- [ ] **Step 3: Add `push_no_drift_when_only_modified_at_differs` to `tests/cli_push.rs`**

```rust
#[tokio::test]
async fn push_no_drift_when_only_modified_at_differs() {
    let server = wiremock::MockServer::start().await;
    let dir = tempfile::TempDir::new().unwrap();
    // Fixture: local label edited (real content change), lockfile matches
    // pre-edit content_hash, mock GET returns object with same content as
    // base BUT a newer modified_at, mock PATCH returns 200.

    let label_id = 42u64;
    let label_url = format!("{}/api/v1/labels/{label_id}", server.uri());
    let remote_with_new_modified_at = serde_json::json!({
        "id": label_id,
        "url": label_url,
        "name": "audit-hold",
        "queues": [],
        "modified_at": "2026-12-31T23:59:59Z"
    });
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!("/api/v1/labels/{label_id}")))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&remote_with_new_modified_at))
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("PATCH"))
        .and(wiremock::matchers::path(format!("/api/v1/labels/{label_id}")))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&remote_with_new_modified_at))
        .mount(&server)
        .await;

    let mut cmd = assert_cmd::Command::cargo_bin("rdc").unwrap();
    let out = cmd
        .args(["push", "test"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("drifted"),
        "expected no drift refusal on modified_at-only difference. stderr: {stderr}"
    );
    assert!(
        stderr.contains("✓ labels:") && stderr.contains("1 patched"),
        "expected one label patched. stderr: {stderr}"
    );
}
```

- [ ] **Step 4: Run the integration tests**

Run: `cargo test --test cli_pull pull_with_orphan_queue_surfaces` and `cargo test --test cli_push push_with_no_changes_prints_no_bars push_no_drift_when_only_modified_at_differs`
Expected: PASS for all three.

- [ ] **Step 5: Run the full test suite**

Run: `cargo test`
Expected: PASS — full suite green. Final new-test count is roughly ~25 (5 noise + 4 hash + 2 PullAction + 1 resolver-noise + 7 colorize/prompt + 3 progress format + 3 integration).

- [ ] **Step 6: Commit**

```bash
git add tests/cli_pull.rs tests/cli_push.rs
git commit -m "$(cat <<'EOF'
test: integration coverage for orphan summary, no-change push, push noise

Three integration tests exercise the new UX end-to-end: pull orphan queue
surfaces an orphans-skipped count in the queues done-line; push with no
changes exits with one summary line and no phase-2 bars; push with only
modified_at differing on remote does not register as drift.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 23: README updates

**Why:** Document the new behavior so users know what to expect (especially the one-time hash-algorithm migration and the `rdc repair` recovery path).

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add an "Upgrading older lockfiles" section**

Insert near the top of the README, after the install / quick-start section:

```markdown
## Upgrading older lockfiles

This release changes how `content_hash` is computed: server-managed fields
(`modified_at`, `modifier`) are now stripped before hashing. The first
pull after upgrade may surface false-positive conflicts on every object
because the lockfile was written with the old algorithm.

To clear the storm without resolving each conflict by hand:

    rdc repair --rebuild-lock <env>

Subsequent pulls will be clean. Real edits made before upgrading remain
visible — `repair` only re-baselines the hash; it does not discard
local edits.
```

- [ ] **Step 2: Update the pull/push output examples**

Find the existing pull/push section in the README and update its sample output to reflect the new `→` / `✓` shape. Match the format from the spec's User-visible behavior section.

- [ ] **Step 3: Add a "Conflict colors" subsection**

Under the existing conflict-handling section, add:

```markdown
### Conflict colors

When run in a TTY, `rdc pull` colorizes conflict prompts: the header is
bold yellow, `-` (local) lines are red, `+` (remote) lines are green,
hunk markers are cyan, and action letters (`[k]/[r]/[e]/[s]/[a]`) are
bold cyan. To force plain output, set `NO_COLOR=1` or pass `--no-color`.
```

- [ ] **Step 4: Update the tagline at the top**

If the README has an old milestone status line, replace it with a stable project description (e.g. "Rossum Deployment as Code — snapshot, edit, and deploy Rossum configurations reliably.").

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "$(cat <<'EOF'
docs: document UX hardening (progress bars, colors, noise suppression)

Adds upgrade note pointing at rdc repair --rebuild-lock for the
one-time hash-algorithm migration. Updates pull/push output samples to
the new ✓-line shape. Documents NO_COLOR and conflict color scheme.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 24: Live verification against @mrtnzlml sandbox

**Why:** Spec acceptance criteria 1-9 all need real-world confirmation.

**Files:** none — verification only.

- [ ] **Step 1: Set up a fresh sandbox project**

```bash
mkdir -p /tmp/rdc-m33-verify
cd /tmp/rdc-m33-verify
rdc init --name verify --env dev
echo "<your-token>" | rdc auth dev
```

- [ ] **Step 2: TTY pull**

Run: `rdc pull dev`
Expected: per-kind progress bars render with ETAs; `→` / `✓` lines for each kind; final `✓ pull envs/dev: ... items, ...` summary. No torn lines, no orphan eprintlns.

- [ ] **Step 3: Non-TTY pull**

Run: `rdc pull dev 2>&1 | tee /tmp/rdc-pull.log`
Expected: log-mode output (no escape codes). One `→` and one `✓` line per kind. Inspect `/tmp/rdc-pull.log` to confirm.

- [ ] **Step 4: Re-pull (idempotency check)**

Run: `rdc pull dev` again immediately.
Expected: zero conflicts. Per-kind done-lines show `0 conflicts` (or omit the conflict count). No `.remote` files anywhere.

- [ ] **Step 5: Trigger a real conflict and confirm color**

Edit one local hook's description in an editor. Tamper its lockfile content_hash to be different from both local and remote (force conflict). Run `rdc pull dev`.
Expected: colored prompt — bold yellow header, red `-`, green `+`, cyan `@@`, bold cyan action letters. Pick `[k]eep local`. Pull continues.

- [ ] **Step 6: Plain mode**

Run: `NO_COLOR=1 rdc pull dev` (after re-tampering the lockfile to re-trigger conflict).
Expected: plain prompt — no escape codes — same content otherwise.

- [ ] **Step 7: Push with no changes**

Run: `rdc push dev` immediately after a clean pull.
Expected: single line `✓ push envs/dev: no changes  (256 files scanned, X.Xs)`. No phase-2 bars.

- [ ] **Step 8: Push with one real change**

Edit a hook's description; run `rdc push dev`.
Expected: `→ push envs/dev: hashing…` then `✓ push envs/dev: 256 files scanned, 1 changed`, then `✓ hooks: 1 patched X.Xs` bar, then final summary line. Single PATCH, not 256.

- [ ] **Step 9: Pull twice, no remote change → file mtime preserved**

```bash
stat -f "%m" envs/dev/labels/audit-hold.json    # capture mtime
rdc pull dev
stat -f "%m" envs/dev/labels/audit-hold.json    # verify unchanged
```

Expected: identical mtime — `NoChange` action skipped the rewrite.

- [ ] **Step 10: Document any deviations**

If any acceptance criterion fails, file a TODO at the top of `docs/superpowers/specs/2026-05-07-pull-push-progress-bar-design.md` and address before merging. If all pass: ready to merge.

- [ ] **Step 11: Capture the verified state in project memory**

Update `~/.claude/projects/-Users-martin-zlamal-rossum-ai-Work-github-com-mrtnzlml-rossum-deployment-manager-experiment/memory/project_rdc.md` with a status section summarizing what shipped, the test count, and live-verification results. (This is a memory write — see superpowers:using-superpowers if needed.)

- [ ] **Step 12: Final commit (memory only — code already committed across earlier tasks)**

No code commit here — Tasks 1-23 produced the shippable diff. This task is verification + documentation only.

---

## Self-Review

After writing the plan, the spec → plan coverage check:

- ✅ Spec §1 (per-kind bar UX) — Tasks 11-18, 20.
- ✅ Spec §2 (orphan-count surface) — Tasks 16, 18; integration test in Task 22.
- ✅ Spec §3 (colorize resolver) — Tasks 7-9; live verification in Task 24 step 5-6.
- ✅ Spec §4 (noise-field suppression) — Tasks 1-6; live verification in Task 24 step 4.
- ✅ Spec §5 (NO_COLOR / --no-color honoring) — Task 8 `detect_color_mode`; Task 24 step 6.
- ✅ Spec §6 (TTY auto-fallback / log mode) — Task 11 `KindProgress::start`; Task 24 step 3.
- ✅ Spec §7 (push two-phase) — Tasks 19-20; Task 24 steps 7-8.
- ✅ Spec §8 (NoChange / disk byte-stability) — Task 4; Task 24 step 9.
- ✅ Spec §9 (warnings via suspend) — Tasks 17, 21.
- ✅ Spec §10 (acceptance criteria 1-9) — verified in Task 24.
- ✅ One-time hash-migration note — README in Task 23, recovery via existing `rdc repair --rebuild-lock`.

**Type consistency check:**
- `KindProgress` signature consistent across Tasks 11-18, 21 (always `&KindProgress`).
- `PullAction::NoChange` defined in Task 4, referenced in Task 17.
- `ChangeList` defined in Task 19, consumed in Task 20.
- `colorize_diff_line(line, mode)` consistent across Tasks 8-9.

**Placeholder scan:** all `todo!()` blocks in Task 19 step 1 are flagged as scaffold only and are filled in Step 2-3. No "TBD" / "implement later" / unsigned-off-on patterns remain.

**Scope check:** three phases, each independently shippable. Plan ordered so each phase builds on previous; can also be merged together.

---

## Appendix — Ordering rationale

Phase 1 (noise) ships first because:
- It's pure infrastructure (no UI churn) — easy to review.
- It eliminates the most visible user pain (spurious conflicts) immediately.
- It makes Phase 3's progress UX testing easier (no fake conflicts polluting output).

Phase 2 (colors) ships second because:
- Tiny scope. Doesn't depend on Phase 3.
- Can ship independently if Phase 3 hits a snag.

Phase 3 (progress bars) ships last because:
- Largest diff (touches every pull/push driver).
- Naturally builds on Phase 1's quieter conflict semantics.
- Retry-warning routing in Task 21 needs the `KindProgress` infrastructure that doesn't exist before Phase 3.
