# Sync "apply to all remaining" conflict resolution — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the operator, from inside an `rdc sync` conflict prompt, press `K`/`R` to resolve every remaining conflict the same way (keep-local-for-all / use-env-for-all) across both the content-conflict and delete-conflict phases, after one bulk confirmation.

**Architecture:** Two new `Resolution` variants (`KeepLocalAll`/`KeepRemoteAll`) are returned by the two core prompt functions when the user picks the uppercase option and confirms. The sync executor owns a `bulk_sticky: Option<BulkChoice>` for the run, threaded by `&mut` into both resolution loops; once set, subsequent conflicts skip the prompt and apply the mapped single-item resolution. The bulk options are only offered (and uppercase repurposed) when >1 prompted conflict remains.

**Tech Stack:** Rust, `cargo test` (unit tests inline in modules), `inquire` is NOT used for these prompts — they are a custom `BufRead`/`Write` loop, tested by injecting `std::io::Cursor`.

## Global Constraints

- **No customer identifiers** anywhere — code, tests, fixtures, commit messages. Use neutral placeholders (`prod`, `test`, `audit-hold`, `acme`). (CLAUDE.md)
- **Backward compatibility:** lowercase `k`/`r`/`s`/`a`/`e`/`h` behavior is unchanged. Uppercase `K`/`R` are repurposed **only when bulk options are shown** (`bulk.is_some()`); when hidden, `K`/`R` keep today's case-insensitive single-item behavior. `e`/`h`/`s`/`a` keep case-insensitivity always.
- **Spec:** `docs/superpowers/specs/2026-06-23-sync-apply-to-all-conflict-resolution-design.md`.
- **Scope:** sticky covers exactly `BothDiverged` (content phase) + `LocalEditRemoteDelete` + `LocalDeleteRemoteEdit` (delete phase). `RemoteDelete` (even drifted), `BothDeleted`, and `prune_mdh_orphans` stay outside the bulk mechanism.
- **Non-interactive (`--yes`/non-TTY) unchanged:** no prompts fire, so the sticky never engages.
- **Verification commands (run all before final commit):** `cargo build`, `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`.
- Per-conflict resolution mapping (existing semantics the sticky auto-selects):
  | Class | `R` / AllRemote | `K` / AllLocal |
  |---|---|---|
  | `BothDiverged` | overwrite local content | keep local, push it |
  | `LocalEditRemoteDelete` | delete local file | restore object on env |
  | `LocalDeleteRemoteEdit` | recreate local from env | commit local tombstone (deferred, needs `rdc push --allow-deletes`) |

---

### Task 1: Core types — `BulkChoice`, `BulkPrompt`, `Resolution` variants

**Files:**
- Modify: `src/cli/resolve.rs` (the `Resolution` enum at ~54; add types after it)
- Modify: `src/cli/sync/execute.rs` (3 exhaustive `match resolution` sites: ~1457, ~1953, ~2747 — add exhaustiveness arms)
- Test: `src/cli/resolve.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub enum BulkChoice { AllLocal, AllRemote }` with `pub fn resolution(self) -> Resolution` (`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`).
  - `pub struct BulkPrompt { pub keep_local_summary: String, pub use_remote_summary: String }`.
  - `Resolution::KeepLocalAll` and `Resolution::KeepRemoteAll` variants.

- [ ] **Step 1: Write the failing test** (append to the `mod tests` in `src/cli/resolve.rs`)

```rust
#[test]
fn bulk_choice_maps_to_single_item_resolution() {
    assert!(matches!(BulkChoice::AllLocal.resolution(), Resolution::KeepLocal));
    assert!(matches!(BulkChoice::AllRemote.resolution(), Resolution::KeepRemote));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test bulk_choice_maps_to_single_item_resolution`
Expected: FAIL — `cannot find type BulkChoice` / does not compile.

- [ ] **Step 3: Add the variants and types**

In `src/cli/resolve.rs`, add two variants to `enum Resolution` (after `EditWithMarkers`, before `Skip`):

```rust
    /// User chose "[K] keep local for ALL remaining conflicts" and confirmed.
    /// The executor normalizes this to `KeepLocal` for the current item and
    /// records a sticky `BulkChoice::AllLocal` so the rest of the run skips
    /// prompting. Never reaches the executor's `match resolution` arms.
    KeepLocalAll,
    /// User chose "[R] use {env} for ALL remaining conflicts" and confirmed.
    /// Normalized to `KeepRemote` + sticky `BulkChoice::AllRemote`.
    KeepRemoteAll,
```

Immediately after the `enum Resolution { ... }` block, add:

```rust
/// Which side an in-prompt "apply to all remaining" choice takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkChoice {
    AllLocal,
    AllRemote,
}

impl BulkChoice {
    /// The single-item resolution this bulk choice maps onto for each
    /// remaining conflict.
    pub fn resolution(self) -> Resolution {
        match self {
            BulkChoice::AllLocal => Resolution::KeepLocal,
            BulkChoice::AllRemote => Resolution::KeepRemote,
        }
    }
}

/// Supplied by the sync executor to enable the in-prompt "apply to all
/// remaining" escape hatch. When passed `Some`, the prompt shows
/// `[K]`/`[R]` and, on selection, prints the matching summary + a
/// `Continue? [y/N]` confirmation before returning `KeepLocalAll` /
/// `KeepRemoteAll`. When `None`, the options are hidden and `K`/`R`
/// keep their existing single-item behavior.
pub struct BulkPrompt {
    /// Multi-line impact summary shown before the confirmation when the
    /// user picks `[K]` (keep local for all). No trailing newline.
    pub keep_local_summary: String,
    /// Same, for `[R]` (use env for all).
    pub use_remote_summary: String,
}
```

- [ ] **Step 4: Restore exhaustiveness in the 3 executor matches**

In `src/cli/sync/execute.rs`, each of the three `match resolution { ... }` blocks is now non-exhaustive. Add this arm to each (the executor normalizes `*All` away before the match, so these are invariant guards):

At `resolve_one_conflict`'s match (~line 1457, the block starting `match resolution {` with a `Resolution::KeepLocal =>` arm) — add before the closing brace:

```rust
        Resolution::KeepLocalAll | Resolution::KeepRemoteAll => {
            unreachable!("bulk *All resolutions are normalized to KeepLocal/KeepRemote before this match")
        }
```

At `prune_mdh_orphans`'s match (~line 1953, has `Resolution::Edit(_) | Resolution::EditWithMarkers(_) | Resolution::Abort =>` arm) — extend that arm's pattern to include the new variants (MDH never offers bulk, so they are equally unreachable here and the existing arm returns `PullAborted`):

```rust
            Resolution::Edit(_)
            | Resolution::EditWithMarkers(_)
            | Resolution::Abort
            | Resolution::KeepLocalAll
            | Resolution::KeepRemoteAll => {
                return Err(anyhow::Error::new(PullAborted));
            }
```

At `resolve_remote_deletes`'s match (~line 2842, same `Resolution::Edit(_) | ...Abort =>` shape) — extend the same way:

```rust
                    Resolution::Edit(_)
                    | Resolution::EditWithMarkers(_)
                    | Resolution::Abort
                    | Resolution::KeepLocalAll
                    | Resolution::KeepRemoteAll => {
                        return Err(anyhow::Error::new(PullAborted));
                    }
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test bulk_choice_maps_to_single_item_resolution && cargo build`
Expected: PASS; build succeeds.

- [ ] **Step 6: Commit**

```bash
git add src/cli/resolve.rs src/cli/sync/execute.rs
git commit -m "feat(sync): add BulkChoice/BulkPrompt types and KeepLocalAll/KeepRemoteAll resolutions

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Content prompt learns the bulk option

**Files:**
- Modify: `src/cli/resolve.rs` — `prompt_resolve_with_bytes_and_color` (~220) gains `bulk: Option<&BulkPrompt>`; wrappers `prompt_resolve` (~133), `prompt_resolve_with_color` (~157), `prompt_resolve_with_bytes` (~191) forward `None`.
- Test: `src/cli/resolve.rs` `mod tests`.

**Interfaces:**
- Consumes: `BulkPrompt`, `Resolution::{KeepLocalAll,KeepRemoteAll}` (Task 1).
- Produces: `prompt_resolve_with_bytes_and_color(input, output, index, total, local_path, local_bytes, remote_bytes, env, mode, bulk: Option<&BulkPrompt>) -> Result<Resolution>`. The 3 wrapper signatures are **unchanged** (they pass `None` internally), so existing tests/callers of the wrappers don't change.

- [ ] **Step 1: Write the failing tests** (append to `mod tests` in `src/cli/resolve.rs`)

```rust
#[test]
fn prompt_bulk_use_remote_all_after_confirm() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("x.json");
    let bulk = BulkPrompt {
        keep_local_summary: "KEEP-ALL-SUMMARY".to_string(),
        use_remote_summary: "USE-ALL-SUMMARY".to_string(),
    };
    let input = Cursor::new(b"R\ny\n");
    let mut out: Vec<u8> = Vec::new();
    let r = prompt_resolve_with_bytes_and_color(
        input, &mut out, 1, 3, &path,
        b"{\"a\":1}", b"{\"a\":2}", "prod", ColorMode::Plain, Some(&bulk),
    )
    .unwrap();
    assert!(matches!(r, Resolution::KeepRemoteAll));
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("[R] use prod for ALL"), "options not shown: {s}");
    assert!(s.contains("USE-ALL-SUMMARY"), "summary not shown: {s}");
}

#[test]
fn prompt_bulk_keep_local_all_after_confirm() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("x.json");
    let bulk = BulkPrompt {
        keep_local_summary: "KEEP-ALL-SUMMARY".to_string(),
        use_remote_summary: "USE-ALL-SUMMARY".to_string(),
    };
    let input = Cursor::new(b"K\ny\n");
    let mut out: Vec<u8> = Vec::new();
    let r = prompt_resolve_with_bytes_and_color(
        input, &mut out, 1, 3, &path,
        b"{\"a\":1}", b"{\"a\":2}", "prod", ColorMode::Plain, Some(&bulk),
    )
    .unwrap();
    assert!(matches!(r, Resolution::KeepLocalAll));
}

#[test]
fn prompt_bulk_declined_falls_back_to_single() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("x.json");
    let bulk = BulkPrompt {
        keep_local_summary: "KEEP-ALL-SUMMARY".to_string(),
        use_remote_summary: "USE-ALL-SUMMARY".to_string(),
    };
    // R -> confirm -> n (decline) -> reprompt -> k (single keep local)
    let input = Cursor::new(b"R\nn\nk\n");
    let mut out: Vec<u8> = Vec::new();
    let r = prompt_resolve_with_bytes_and_color(
        input, &mut out, 1, 3, &path,
        b"{\"a\":1}", b"{\"a\":2}", "prod", ColorMode::Plain, Some(&bulk),
    )
    .unwrap();
    assert!(matches!(r, Resolution::KeepLocal));
}

#[test]
fn prompt_bulk_none_preserves_uppercase_single_behavior() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("x.json");
    let input = Cursor::new(b"R\n");
    let mut out: Vec<u8> = Vec::new();
    let r = prompt_resolve_with_bytes_and_color(
        input, &mut out, 1, 1, &path,
        b"{\"a\":1}", b"{\"a\":2}", "prod", ColorMode::Plain, None,
    )
    .unwrap();
    assert!(matches!(r, Resolution::KeepRemote), "uppercase R must still mean single KeepRemote when bulk is None");
    let s = String::from_utf8(out).unwrap();
    assert!(!s.contains("for ALL"), "bulk options must be hidden when None: {s}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test prompt_bulk_`
Expected: FAIL — `prompt_resolve_with_bytes_and_color` takes 9 args, not 10 (does not compile).

- [ ] **Step 3: Add the `bulk` parameter and behavior**

In `prompt_resolve_with_bytes_and_color`, change the signature to add a trailing parameter:

```rust
pub fn prompt_resolve_with_bytes_and_color<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    index: usize,
    total: usize,
    local_path: &Path,
    local_bytes: &[u8],
    remote_bytes: &[u8],
    env: &str,
    mode: ColorMode,
    bulk: Option<&BulkPrompt>,
) -> Result<Resolution> {
```

Inside the `loop {` (currently ~287), after computing `prompt_text` and before the `write!(output, ...)` for it, emit the all-options line when offered:

```rust
        if bulk.is_some() {
            writeln!(
                output,
                "{}",
                colorize_prompt(&format!("[K] keep ALL local  [R] use {env} for ALL"), mode)
            )?;
        }
```

Then, in the `match line.trim().chars().next() {` block, add these two arms **before** the existing `Some('k') | Some('K') => ...` arm (so `K`/`R` only divert to bulk when offered; otherwise they fall through to the existing single-item arms):

```rust
            Some('K') if bulk.is_some() => {
                let b = bulk.expect("checked is_some");
                writeln!(output, "{}", b.keep_local_summary)?;
                write!(output, "{}", colorize_prompt("Continue? [y/N] > ", mode))?;
                output.flush().ok();
                let mut c = String::new();
                if input.read_line(&mut c)? == 0 {
                    return Ok(Resolution::Skip);
                }
                if matches!(c.trim().chars().next(), Some('y') | Some('Y')) {
                    return Ok(Resolution::KeepLocalAll);
                }
                continue;
            }
            Some('R') if bulk.is_some() => {
                let b = bulk.expect("checked is_some");
                writeln!(output, "{}", b.use_remote_summary)?;
                write!(output, "{}", colorize_prompt("Continue? [y/N] > ", mode))?;
                output.flush().ok();
                let mut c = String::new();
                if input.read_line(&mut c)? == 0 {
                    return Ok(Resolution::Skip);
                }
                if matches!(c.trim().chars().next(), Some('y') | Some('Y')) {
                    return Ok(Resolution::KeepRemoteAll);
                }
                continue;
            }
```

- [ ] **Step 4: Forward `None` from the three wrappers**

`prompt_resolve_with_color` (~157) calls `prompt_resolve_with_bytes_and_color(...)` at ~168 — add `None` as the final argument. `prompt_resolve_with_bytes` (~191) calls it at ~202 — add `None`. `prompt_resolve` (~133) calls `prompt_resolve_with_color` (a wrapper, unchanged) — **no change needed**.

```rust
    // in prompt_resolve_with_color (~168) and prompt_resolve_with_bytes (~202):
    prompt_resolve_with_bytes_and_color(
        input, output, index, total, local_path, /* local_bytes, */ remote_bytes, env, mode,
        None,
    )
```
(Append `None` to each existing call — keep all current arguments.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test prompt_bulk_ && cargo test --lib resolve && cargo build`
Expected: PASS; existing resolve tests still pass (wrappers unchanged); build succeeds.

- [ ] **Step 6: Commit**

```bash
git add src/cli/resolve.rs
git commit -m "feat(sync): content conflict prompt offers [K]/[R] apply-to-all with confirm

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Delete prompt learns the bulk option

**Files:**
- Modify: `src/cli/resolve.rs` — `prompt_remote_delete_with_color` (~389) gains `bulk: Option<&BulkPrompt>`; wrapper `prompt_remote_delete` (~377) forwards `None`; the 4 existing direct test callers (~2645, ~2677, ~2691, ~2705) append `None`.
- Test: `src/cli/resolve.rs` `mod tests`.

**Interfaces:**
- Consumes: `BulkPrompt`, `Resolution::{KeepLocalAll,KeepRemoteAll}`.
- Produces: `prompt_remote_delete_with_color(input, output, local_path, env, mode, bulk: Option<&BulkPrompt>) -> Result<Resolution>`. Wrapper `prompt_remote_delete` signature unchanged.

- [ ] **Step 1: Write the failing tests** (append to `mod tests`)

```rust
#[test]
fn prompt_remote_delete_bulk_use_remote_all() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("x.json");
    std::fs::write(&path, b"{\"a\":1}\n").unwrap();
    let bulk = BulkPrompt {
        keep_local_summary: "KEEP-ALL".to_string(),
        use_remote_summary: "USE-ALL".to_string(),
    };
    let input = Cursor::new(b"R\ny\n");
    let mut out: Vec<u8> = Vec::new();
    let r = prompt_remote_delete_with_color(input, &mut out, &path, "prod", ColorMode::Plain, Some(&bulk)).unwrap();
    assert!(matches!(r, Resolution::KeepRemoteAll));
    assert!(String::from_utf8(out).unwrap().contains("[R] use prod for ALL"));
}

#[test]
fn prompt_remote_delete_bulk_none_preserves_single() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("x.json");
    std::fs::write(&path, b"{\"a\":1}\n").unwrap();
    let input = Cursor::new(b"r\n");
    let mut out: Vec<u8> = Vec::new();
    let r = prompt_remote_delete_with_color(input, &mut out, &path, "prod", ColorMode::Plain, None).unwrap();
    assert!(matches!(r, Resolution::KeepRemote));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test prompt_remote_delete_bulk_`
Expected: FAIL — arity mismatch, does not compile.

- [ ] **Step 3: Add the `bulk` parameter and behavior**

Change `prompt_remote_delete_with_color` signature to add a trailing `bulk: Option<&BulkPrompt>`:

```rust
pub fn prompt_remote_delete_with_color<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    local_path: &Path,
    env: &str,
    mode: ColorMode,
    bulk: Option<&BulkPrompt>,
) -> Result<Resolution> {
```

Inside its `loop {` (~424), after building `prompt_text` and before `write!(output, ...)`, add the all-options line:

```rust
        if bulk.is_some() {
            writeln!(
                output,
                "{}",
                colorize_prompt(&format!("[K] keep ALL local  [R] use {env} for ALL"), mode)
            )?;
        }
```

In its `match line.trim().chars().next() {` block, add the two arms **before** the existing `Some('k') | Some('K') => ...` arm (identical confirm logic as Task 2):

```rust
            Some('K') if bulk.is_some() => {
                let b = bulk.expect("checked is_some");
                writeln!(output, "{}", b.keep_local_summary)?;
                write!(output, "{}", colorize_prompt("Continue? [y/N] > ", mode))?;
                output.flush().ok();
                let mut c = String::new();
                if input.read_line(&mut c)? == 0 {
                    return Ok(Resolution::Skip);
                }
                if matches!(c.trim().chars().next(), Some('y') | Some('Y')) {
                    return Ok(Resolution::KeepLocalAll);
                }
                continue;
            }
            Some('R') if bulk.is_some() => {
                let b = bulk.expect("checked is_some");
                writeln!(output, "{}", b.use_remote_summary)?;
                write!(output, "{}", colorize_prompt("Continue? [y/N] > ", mode))?;
                output.flush().ok();
                let mut c = String::new();
                if input.read_line(&mut c)? == 0 {
                    return Ok(Resolution::Skip);
                }
                if matches!(c.trim().chars().next(), Some('y') | Some('Y')) {
                    return Ok(Resolution::KeepRemoteAll);
                }
                continue;
            }
```

- [ ] **Step 4: Forward `None` from the wrapper and existing test callers**

In `prompt_remote_delete` (~384), append `None` to the call:

```rust
    prompt_remote_delete_with_color(input, output, local_path, env, mode, None)
```

In `src/cli/resolve.rs` `mod tests`, the 4 existing direct calls at ~2645, ~2677, ~2691, ~2705 each need `None` appended as the final argument. (Grep to confirm: `grep -n "prompt_remote_delete_with_color(" src/cli/resolve.rs` — update every call that is not the wrapper definition.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test prompt_remote_delete && cargo build`
Expected: PASS; build succeeds.

- [ ] **Step 6: Commit**

```bash
git add src/cli/resolve.rs
git commit -m "feat(sync): remote-delete prompt offers [K]/[R] apply-to-all with confirm

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Wire the content phase (`build_bulk_prompt`, `resolve_conflicts`, `resolve_one_conflict`, run owns sticky)

**Files:**
- Modify: `src/cli/sync/execute.rs` — add `build_bulk_prompt`; thread `bulk_sticky` into `resolve_conflicts` (~81) and `resolve_one_conflict` (~1033); rewire the 3 prompt sites (~1356, ~1405, ~1428) to call the core prompt with `bulk`; normalize at the choke point (~1448); create `bulk_sticky` in `run` (~2966); update the 8 `resolve_conflicts` test call sites.
- Modify import at `src/cli/sync/execute.rs:37` to bring in the new names.
- Test: `src/cli/sync/execute.rs` `mod tests`.

**Interfaces:**
- Consumes: `BulkChoice`, `BulkPrompt`, `prompt_resolve_with_bytes_and_color`, `Resolution::{KeepLocalAll,KeepRemoteAll}`, `crate::cli::resolve::detect_color_mode`.
- Produces:
  - `fn build_bulk_prompt(env: &str, content: usize, delete_local: usize, recreate_local: usize) -> Option<BulkPrompt>` — `None` when `content + delete_local + recreate_local <= 1`.
  - `resolve_conflicts(ctx, catalog, classified, input, interactive, progress, bulk_sticky: &mut Option<BulkChoice>)`.
  - `resolve_one_conflict(ctx, it, refs, idx_one_based, total, input, stderr_lock, interactive, env, progress, outcome, bulk_sticky: &mut Option<BulkChoice>, bulk: Option<&BulkPrompt>)`.

- [ ] **Step 1: Update imports**

In `src/cli/sync/execute.rs:37`, extend the resolve import:

```rust
use crate::cli::resolve::{
    BulkChoice, BulkPrompt, PullAborted, Resolution, detect_color_mode, prompt_remote_delete,
    prompt_resolve, prompt_resolve_with_bytes_and_color,
};
```
(Keep whatever is already imported; add `BulkChoice`, `BulkPrompt`, `detect_color_mode`, `prompt_resolve_with_bytes_and_color`. `prompt_resolve`/`prompt_remote_delete` stay for `prune_mdh_orphans` and any unchanged sites.)

- [ ] **Step 2: Write the failing unit test for `build_bulk_prompt`** (in `mod tests`)

```rust
#[test]
fn build_bulk_prompt_hides_when_single() {
    assert!(build_bulk_prompt("prod", 1, 0, 0).is_none());
    assert!(build_bulk_prompt("prod", 0, 1, 0).is_none());
    assert!(build_bulk_prompt("prod", 0, 0, 0).is_none());
}

#[test]
fn build_bulk_prompt_lists_nonzero_classes() {
    let b = build_bulk_prompt("prod", 39, 2, 1).expect("should offer when >1");
    assert!(b.use_remote_summary.contains("all 42 remaining"));
    assert!(b.use_remote_summary.contains("39 local file(s) overwritten with prod"));
    assert!(b.use_remote_summary.contains("2 local file(s) deleted"));
    assert!(b.use_remote_summary.contains("1 local file(s) recreated from prod"));
    assert!(b.keep_local_summary.contains("39 local file(s) kept and pushed to prod"));
    assert!(b.keep_local_summary.contains("rdc push --allow-deletes prod"));
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test build_bulk_prompt_`
Expected: FAIL — `cannot find function build_bulk_prompt`.

- [ ] **Step 4: Add `build_bulk_prompt`** (place it as a free fn near `resolve_conflicts`, e.g. just above it ~line 80)

```rust
/// Build the two confirmation summaries for the in-prompt "apply to all
/// remaining" escape hatch, given the remaining prompted-conflict counts by
/// class. Returns `None` when one or fewer conflicts remain — offering "all"
/// would be identical to resolving the single item, so the options stay
/// hidden and uppercase `K`/`R` keep their single-item meaning.
fn build_bulk_prompt(
    env: &str,
    content: usize,        // BothDiverged remaining, including the current one
    delete_local: usize,   // LocalEditRemoteDelete remaining
    recreate_local: usize, // LocalDeleteRemoteEdit remaining
) -> Option<BulkPrompt> {
    let total = content + delete_local + recreate_local;
    if total <= 1 {
        return None;
    }
    let mut keep = format!("Keep local for all {total} remaining conflicts?");
    let mut remote = format!("Use {env} for all {total} remaining conflicts?");
    if content > 0 {
        keep.push_str(&format!("\n  {content} local file(s) kept and pushed to {env}"));
        remote.push_str(&format!("\n  {content} local file(s) overwritten with {env}"));
    }
    if delete_local > 0 {
        keep.push_str(&format!("\n  {delete_local} file(s) restored on {env}"));
        remote.push_str(&format!(
            "\n  {delete_local} local file(s) deleted ({env} deleted them)"
        ));
    }
    if recreate_local > 0 {
        keep.push_str(&format!(
            "\n  {recreate_local} local deletion(s) need a follow-up `rdc push --allow-deletes {env}`"
        ));
        remote.push_str(&format!(
            "\n  {recreate_local} local file(s) recreated from {env}"
        ));
    }
    Some(BulkPrompt { keep_local_summary: keep, use_remote_summary: remote })
}
```

- [ ] **Step 5: Run the unit test to verify it passes**

Run: `cargo test build_bulk_prompt_`
Expected: PASS.

- [ ] **Step 6: Thread `bulk_sticky` + `bulk` through `resolve_conflicts` and `resolve_one_conflict`**

(a) `resolve_conflicts` signature (~81) — add the trailing param:

```rust
pub(crate) async fn resolve_conflicts<R: BufRead>(
    ctx: &mut PullCtx<'_>,
    catalog: &RemoteCatalog,
    classified: &[ClassifiedItem],
    mut input: R,
    interactive: bool,
    progress: &Arc<Log>,
    bulk_sticky: &mut Option<BulkChoice>,
) -> Result<ConflictOutcome> {
```

(b) Just after `let total = conflicts.len();` and the `if total == 0 { return Ok(outcome); }` early-return (~268), precompute the constant delete-phase counts:

```rust
    let lerd_total = classified
        .iter()
        .filter(|it| it.class == SyncClass::LocalEditRemoteDelete)
        .count();
    let ldre_total = classified
        .iter()
        .filter(|it| it.class == SyncClass::LocalDeleteRemoteEdit)
        .count();
```

(c) Inside the loop `for (idx, it) in conflicts.iter().enumerate() {` (~276), after `refs` is resolved and just before the `resolve_one_conflict(...)` call, compute the per-item bulk prompt and pass it plus the sticky. `idx` is 0-based, so content conflicts remaining (including current) = `total - idx`:

```rust
        let content_remaining = total - idx;
        let bulk = build_bulk_prompt(&env, content_remaining, lerd_total, ldre_total);
        resolve_one_conflict(
            ctx,
            it,
            refs,
            idx + 1,
            total,
            &mut input,
            &mut stderr_lock,
            interactive,
            &env,
            progress,
            &mut outcome,
            bulk_sticky,
            bulk.as_ref(),
        )?;
```
(Match the existing argument list; only the two trailing args are new. Keep the existing `idx + 1`, `&mut input`, etc. exactly as they are now.)

(d) `resolve_one_conflict` signature (~1033) — add the two trailing params:

```rust
fn resolve_one_conflict<R: BufRead>(
    ctx: &mut PullCtx<'_>,
    it: &ClassifiedItem,
    refs: ConflictRefs,
    idx_one_based: usize,
    total: usize,
    input: &mut R,
    stderr_lock: &mut std::io::StderrLock<'_>,
    interactive: bool,
    env: &str,
    progress: &Arc<Log>,
    outcome: &mut ConflictOutcome,
    bulk_sticky: &mut Option<BulkChoice>,
    bulk: Option<&BulkPrompt>,
) -> Result<()> {
```

(e) Near the top of `resolve_one_conflict`'s prompt section (just before `let prompt_out ...` at ~1348), snapshot the sticky for use inside the closure:

```rust
    let sticky_now: Option<BulkChoice> = *bulk_sticky;
```

(f) Rewire the 3 prompt sites inside the `progress.with_prompt(...)` closure so they honor `sticky_now` and pass `bulk` to the core prompt:

- Site ~1356 (Hook/Rule sidecar). Replace the `let r = crate::cli::resolve::prompt_resolve_with_bytes(...)?;` with:

```rust
                    let r = match sticky_now {
                        Some(b) => b.resolution(),
                        None => prompt_resolve_with_bytes_and_color(
                            &mut *input,
                            &mut *stderr_lock,
                            idx_one_based,
                            total,
                            &code_path,
                            &local_bytes,
                            &remote_bytes_for_prompt,
                            env,
                            detect_color_mode(),
                            bulk,
                        )?,
                    };
```

- Site ~1405 (Schema formula). Replace `let r = crate::cli::resolve::prompt_resolve_with_bytes(...)?;` with:

```rust
                    let r = match sticky_now {
                        Some(b) => b.resolution(),
                        None => prompt_resolve_with_bytes_and_color(
                            &mut *input,
                            &mut *stderr_lock,
                            idx_one_based,
                            total,
                            &formula_path,
                            &local_b,
                            &remote_b,
                            env,
                            detect_color_mode(),
                            bulk,
                        )?,
                    };
```

- Site ~1428 (Flat JSON). Replace `let r = prompt_resolve(...)?;` with (uses `local_json_bytes`, already in scope at ~1102):

```rust
            let r = match sticky_now {
                Some(b) => b.resolution(),
                None => prompt_resolve_with_bytes_and_color(
                    &mut *input,
                    &mut *stderr_lock,
                    idx_one_based,
                    total,
                    &local_path,
                    &local_json_bytes,
                    &remote_bytes,
                    env,
                    detect_color_mode(),
                    bulk,
                )?,
            };
```

(g) Normalize at the choke point. Immediately after the destructuring `let (resolution, code_conflict_only, prompt_local_bytes, prompt_remote_bytes, prompt_path) = prompt_out.into_inner().expect(...);` (~1448), insert:

```rust
    // Bulk "[K]/[R] for ALL": record the sticky for the rest of the run and
    // fold the current item into the equivalent single-item resolution. The
    // `match resolution` below never sees the `*All` variants.
    let resolution = match resolution {
        Resolution::KeepLocalAll => {
            *bulk_sticky = Some(BulkChoice::AllLocal);
            Resolution::KeepLocal
        }
        Resolution::KeepRemoteAll => {
            *bulk_sticky = Some(BulkChoice::AllRemote);
            Resolution::KeepRemote
        }
        other => other,
    };
```
(`resolution` is rebound; the subsequent `match resolution { ... }` is unchanged.)

- [ ] **Step 7: Create the sticky in `run` and pass it to `resolve_conflicts`**

In `run` (~2966), just before the `let conflict_outcome = resolve_conflicts(...)` call, add:

```rust
    // Owned by the run; threaded into both resolution phases so an
    // "apply to all" choice in the content phase carries into the delete
    // phase. (Delete phase is wired in the next task.)
    let mut bulk_sticky: Option<BulkChoice> = None;
```

and append `&mut bulk_sticky` as the final argument of the `resolve_conflicts(...)` call. (Leave the `resolve_remote_deletes(...)` call at ~2980 unchanged for now — Task 5 wires it.)

- [ ] **Step 8: Update the 8 existing `resolve_conflicts` test call sites**

Each call to `resolve_conflicts(&mut ctx, &catalog, &classified, Cursor::new(...), <bool>, &progress)` (at ~3608, 3667, 3732, 3774, 3821, 3904, 3954, 4006) gets a final `&mut None` argument:

```rust
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b"k\n"),
                true,
                &progress,
                &mut None,
            )
```
(If type inference complains on `&mut None`, use a local `let mut sticky = None;` then pass `&mut sticky` — the param type is `&mut Option<BulkChoice>`.)

- [ ] **Step 9: Write the content-phase integration tests** (in `mod tests`)

```rust
/// Picking `R` -> confirm `y` on a content conflict, while another prompted
/// conflict remains (a LERD item appended to `classified` makes the bulk
/// options appear), resolves the current item as use-remote AND sets the
/// run sticky to AllRemote so the rest of the run won't prompt.
#[tokio::test]
async fn resolve_conflicts_use_remote_all_sets_sticky() {
    let mut fixture = setup_conflict_fixture();
    let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
    // One real BothDiverged + one LERD so total remaining > 1 (options shown).
    let mut classified = classified_for(&fixture);
    classified.push(ClassifiedItem {
        kind: "labels".to_string(),
        slug: "other-label".to_string(),
        class: SyncClass::LocalEditRemoteDelete,
        local_hash: None,
        remote_hash: None,
        base_hash: Some("dummy".to_string()),
    });
    let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

    let mut sticky: Option<BulkChoice> = None;
    {
        let mut ctx = PullCtx {
            paths: &fixture.paths,
            client: &fixture.client,
            lockfile: &mut fixture.lockfile,
            queue_locations: BTreeMap::new(),
            interactive: true,
        };
        resolve_conflicts(
            &mut ctx,
            &catalog,
            &classified,
            Cursor::new(b"R\ny\n"),
            true,
            &progress,
            &mut sticky,
        )
        .await
        .expect("resolver should succeed on [R] all");
    }

    assert_eq!(sticky, Some(BulkChoice::AllRemote), "sticky must be set for the rest of the run");
    // Current content conflict resolved as use-remote: local overwritten.
    let remote_bytes = crate::cli::pull::common::portabilize_proposed(
        &label_bytes(&fixture.remote_label),
        &fixture.lockfile,
    );
    let local_after = std::fs::read(&fixture.local_path).unwrap();
    assert_eq!(local_after, remote_bytes, "local must be overwritten with remote on [R] all");
}

/// Declining the bulk confirmation (`R` -> `n`) then picking `k` resolves the
/// single item as keep-local and leaves the sticky unset.
#[tokio::test]
async fn resolve_conflicts_bulk_declined_leaves_sticky_unset() {
    let mut fixture = setup_conflict_fixture();
    let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
    let mut classified = classified_for(&fixture);
    classified.push(ClassifiedItem {
        kind: "labels".to_string(),
        slug: "other-label".to_string(),
        class: SyncClass::LocalEditRemoteDelete,
        local_hash: None,
        remote_hash: None,
        base_hash: Some("dummy".to_string()),
    });
    let progress = Log::new(crate::cli::resolve::ColorMode::Plain);
    let local_before = std::fs::read(&fixture.local_path).unwrap();

    let mut sticky: Option<BulkChoice> = None;
    let outcome = {
        let mut ctx = PullCtx {
            paths: &fixture.paths,
            client: &fixture.client,
            lockfile: &mut fixture.lockfile,
            queue_locations: BTreeMap::new(),
            interactive: true,
        };
        resolve_conflicts(
            &mut ctx,
            &catalog,
            &classified,
            Cursor::new(b"R\nn\nk\n"),
            true,
            &progress,
            &mut sticky,
        )
        .await
        .expect("resolver should succeed after decline + [k]")
    };

    assert_eq!(sticky, None, "declined bulk must not set the sticky");
    assert_eq!(outcome.promoted_to_push.len(), 1, "[k] promotes to push");
    let local_after = std::fs::read(&fixture.local_path).unwrap();
    assert_eq!(local_after, local_before, "local must survive [k]");
}
```

- [ ] **Step 10: Run all affected tests + build**

Run: `cargo test --lib && cargo build`
Expected: PASS — new integration tests pass, all 8 updated call sites compile, existing conflict tests still pass.

- [ ] **Step 11: Commit**

```bash
git add src/cli/sync/execute.rs
git commit -m "feat(sync): content phase honors apply-to-all sticky and offers [K]/[R]

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Wire the delete phase (`resolve_remote_deletes`) + carry sticky across phases

**Files:**
- Modify: `src/cli/sync/execute.rs` — thread `bulk_sticky` into `resolve_remote_deletes` (~2030); gate bulk on `LocalEditRemoteDelete`/`LocalDeleteRemoteEdit`; rewire the prompt site (~2726); normalize at the choke point (~2735); pass `&mut bulk_sticky` from `run` (~2980); update the `resolve_remote_deletes` test call sites.
- Test: `src/cli/sync/execute.rs` `mod tests`.

**Interfaces:**
- Consumes: `build_bulk_prompt`, `BulkChoice`, `prompt_remote_delete_with_color`, `detect_color_mode`, `Resolution::{KeepLocalAll,KeepRemoteAll}`.
- Produces: `resolve_remote_deletes(ctx, catalog, classified, input, interactive, progress, bulk_sticky: &mut Option<BulkChoice>)`.

- [ ] **Step 1: Add `prompt_remote_delete_with_color` to imports**

Extend the resolve import in `src/cli/sync/execute.rs:37` to also bring in `prompt_remote_delete_with_color`.

- [ ] **Step 2: Write the failing cross-phase test** (in `mod tests`)

```rust
/// A sticky `AllRemote` set in the content phase must carry into the delete
/// phase: a LocalEditRemoteDelete item is resolved as use-remote (local file
/// deleted) WITHOUT prompting — proven by feeding empty stdin (a prompt would
/// EOF->Skip, write a marker, and keep the file).
#[tokio::test]
async fn resolve_remote_deletes_honors_sticky_all_remote_without_prompt() {
    let mut fixture = setup_remote_delete_fixture();
    let classified = classified_local_edit_remote_delete();
    let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

    let mut sticky: Option<BulkChoice> = Some(BulkChoice::AllRemote);
    {
        let mut ctx = PullCtx {
            paths: &fixture.paths,
            client: &fixture.client,
            lockfile: &mut fixture.lockfile,
            queue_locations: BTreeMap::new(),
            interactive: true,
        };
        resolve_remote_deletes(
            &mut ctx,
            &catalog_with_labels(vec![]),
            &classified,
            Cursor::new(b""),
            true,
            &progress,
            &mut sticky,
        )
        .await
        .expect("sticky AllRemote must resolve the delete conflict without prompting");
    }

    assert!(!fixture.local_path.exists(), "AllRemote on LERD must delete the local file");
    let marker = deleted_marker_path(&fixture.local_path, "test");
    assert!(!marker.exists(), "no skip marker — the sticky resolved it, no prompt ran");
    assert!(
        fixture.lockfile.objects.get("labels").and_then(|m| m.get("audit-hold")).is_none(),
        "lockfile entry must be dropped on use-remote delete"
    );
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test resolve_remote_deletes_honors_sticky`
Expected: FAIL — arity mismatch (resolve_remote_deletes takes 6 args).

- [ ] **Step 4: Add the param + precompute counts**

`resolve_remote_deletes` signature (~2030) — add trailing `bulk_sticky: &mut Option<BulkChoice>`:

```rust
pub(crate) async fn resolve_remote_deletes<R: BufRead>(
    ctx: &mut PullCtx<'_>,
    catalog: &RemoteCatalog,
    classified: &[ClassifiedItem],
    mut input: R,
    interactive: bool,
    progress: &Arc<Log>,
    bulk_sticky: &mut Option<BulkChoice>,
) -> Result<ConflictOutcome> {
```

Just after `let mut outcome = ConflictOutcome::default();` (~2038), precompute totals and running counters:

```rust
    let lerd_total = classified
        .iter()
        .filter(|it| it.class == SyncClass::LocalEditRemoteDelete)
        .count();
    let ldre_total = classified
        .iter()
        .filter(|it| it.class == SyncClass::LocalDeleteRemoteEdit)
        .count();
    let mut processed_lerd = 0usize;
    let mut processed_ldre = 0usize;
```

- [ ] **Step 5: Rewire the prompt site to honor the sticky and offer bulk**

Replace the block that currently runs the prompt (the `let prompt_res ...; progress.with_prompt(|| { let r = prompt_remote_delete(...)?; ... })?; let resolution = prompt_res.into_inner()...;` at ~2723–2735) with sticky- and bulk-aware logic. `it`/`local_path`/`env` are in scope; `RemoteDelete` (drifted) reaching here is **not** bulk-eligible:

```rust
                let bulk_eligible = matches!(
                    it.class,
                    SyncClass::LocalEditRemoteDelete | SyncClass::LocalDeleteRemoteEdit
                );
                let sticky_now = if bulk_eligible { *bulk_sticky } else { None };

                let resolution = if let Some(b) = sticky_now {
                    b.resolution()
                } else {
                    let bulk = if bulk_eligible {
                        build_bulk_prompt(
                            &env,
                            0,
                            lerd_total - processed_lerd,
                            ldre_total - processed_ldre,
                        )
                    } else {
                        None
                    };
                    let prompt_res: std::cell::RefCell<Option<Resolution>> =
                        std::cell::RefCell::new(None);
                    let local_for_prompt = local_path.clone();
                    progress.with_prompt(|| -> anyhow::Result<()> {
                        let r = prompt_remote_delete_with_color(
                            &mut input,
                            std::io::stderr().lock(),
                            &local_for_prompt,
                            &env,
                            detect_color_mode(),
                            bulk.as_ref(),
                        )?;
                        *prompt_res.borrow_mut() = Some(r);
                        Ok(())
                    })?;
                    let raw = prompt_res.into_inner().expect("with_prompt must populate");
                    match raw {
                        Resolution::KeepLocalAll => {
                            *bulk_sticky = Some(BulkChoice::AllLocal);
                            Resolution::KeepLocal
                        }
                        Resolution::KeepRemoteAll => {
                            *bulk_sticky = Some(BulkChoice::AllRemote);
                            Resolution::KeepRemote
                        }
                        other => other,
                    }
                };

                if bulk_eligible {
                    match it.class {
                        SyncClass::LocalEditRemoteDelete => processed_lerd += 1,
                        SyncClass::LocalDeleteRemoteEdit => processed_ldre += 1,
                        _ => {}
                    }
                }
```

The existing `let is_local_delete_remote_edit = ...;` and the `match resolution { ... }` block that follow stay exactly as they are (they already handle `KeepLocal`/`KeepRemote`/`Skip` and, from Task 1, the unreachable `*All` arm).

- [ ] **Step 6: Pass the sticky from `run`**

In `run` (~2980), append `&mut bulk_sticky` as the final argument to the `resolve_remote_deletes(...)` call (the `bulk_sticky` local was created in Task 4).

- [ ] **Step 7: Update the existing `resolve_remote_deletes` test call sites**

Every existing call to `resolve_remote_deletes(...)` in `mod tests` (~4120, 4169, 4225, 4274, 4750, and any others — confirm with `grep -n "resolve_remote_deletes(" src/cli/sync/execute.rs`) gets a final `&mut None` argument. (As in Task 4, fall back to a `let mut sticky = None; ... &mut sticky` local if inference complains.)

- [ ] **Step 8: Run all affected tests + build**

Run: `cargo test --lib && cargo build`
Expected: PASS — the cross-phase test passes, all updated call sites compile, existing delete tests still pass.

- [ ] **Step 9: Commit**

```bash
git add src/cli/sync/execute.rs
git commit -m "feat(sync): delete phase honors apply-to-all sticky across both phases

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Full verification + manual TTY check

**Files:** none (verification only).

- [ ] **Step 1: Full automated gate**

Run: `cargo test`
Expected: all tests pass.

Run: `cargo clippy --all-targets`
Expected: no warnings introduced by this change.

Run: `cargo fmt --check`
Expected: clean (run `cargo fmt` and amend the last commit if it reformats).

- [ ] **Step 2: Build the release binary for a real TTY check**

Run: `cargo build --release`
Confirm which binary you will run (Homebrew `rdc` can shadow local builds): `which -a rdc` and invoke the local build explicitly as `./target/release/rdc`.

- [ ] **Step 3: Manual PTY verification** (against an env with ≥2 conflicts; `tick_status`/prompt rendering is a no-op off a TTY, so drive it under a PTY)

Run (example):
```bash
script -q /dev/null ./target/release/rdc sync <env>
```
Verify, by eye:
- The conflict prompt shows the extra line `[K] keep ALL local  [R] use <env> for ALL` while >1 conflict remains.
- Pressing `R` prints the impact breakdown and `Continue? [y/N]`; answering `y` resolves every remaining conflict (content and any delete conflicts) without further prompts; answering `n` returns to the per-file prompt.
- Pressing lowercase `r`/`k` still resolves a single conflict as before.
- On the **last** remaining conflict the `[K]`/`[R]` line is absent and uppercase `R`/`K` behave like `r`/`k`.

- [ ] **Step 4: Confirm spec coverage**

Re-read `docs/superpowers/specs/2026-06-23-sync-apply-to-all-conflict-resolution-design.md` and confirm each behavior is implemented. No code change expected here; if a gap is found, open a follow-up task.

---

## Self-Review

**1. Spec coverage:**
- In-prompt `[K]`/`[R]` with confirmation → Tasks 2, 3 (prompt) + 4, 5 (offer/summary).
- Sticky across both phases → Tasks 4 (content) + 5 (delete, cross-phase test).
- Uppercase = all only when offered; hidden → single behavior → Tasks 2, 3 (`if bulk.is_some()` guard arms) + `prompt_bulk_none_preserves_uppercase_single_behavior`.
- Confirmation impact breakdown, zero-count lines omitted → `build_bulk_prompt` + `build_bulk_prompt_lists_nonzero_classes`.
- Hide when only one conflict remains → `build_bulk_prompt` returns `None` + `build_bulk_prompt_hides_when_single`.
- Non-interactive unchanged → no change to the `if !interactive` branches; covered by existing `*_non_interactive` tests still passing.
- `RemoteDelete` drifted + `prune_mdh_orphans` out of scope → Task 5 `bulk_eligible` gate (RemoteDelete excluded); `prune_mdh_orphans` untouched (still calls `prompt_remote_delete` wrapper = `None`).
- Per-class mapping → reuses existing `match resolution` arms unchanged.

**2. Placeholder scan:** No TBD/TODO. Every code step shows complete code; every test step shows full assertions.

**3. Type consistency:** `BulkChoice::resolution`, `BulkPrompt { keep_local_summary, use_remote_summary }`, `build_bulk_prompt(env, content, delete_local, recreate_local) -> Option<BulkPrompt>`, and the `bulk_sticky: &mut Option<BulkChoice>` / `bulk: Option<&BulkPrompt>` parameter names are used identically across Tasks 1–5.
