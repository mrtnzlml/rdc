# rdc diff --raw Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `--raw` flag to `rdc diff` that bypasses diff-time adjustment logic (field-stripping + URL rewriting) while keeping output tidy (key/array sort, sidecar code), across both diff modes.

**Architecture:** A single shared `tidy_raw` normalizer (sort string-arrays + sort keys, strip nothing) lives in `src/snapshot/noise.rs`. The env-vs-env path swaps its normalizer for `tidy_raw` and skips URL rewriting. The local-vs-remote path uses sibling `serialize_*_raw` functions that reuse the existing code/formula split but apply `tidy_raw` instead of the canonical strip/reorder. A `--raw` bool threads from clap down both paths, wired incrementally so every intermediate state compiles.

**Tech Stack:** Rust, clap (derive), serde_json. Tests: `assert_cmd` + `predicates` (CLI), `wiremock` (mock Rossum API), `tempfile`.

**Reference spec:** `docs/superpowers/specs/2026-05-27-diff-raw-flag-design.md`

---

## File Structure

- `src/snapshot/noise.rs` — receives `sort_string_arrays` (moved from `common.rs`) and gains `tidy_raw`. The home for canonical JSON projection primitives.
- `src/cli/deploy/common.rs` — imports `sort_string_arrays` from `noise`; `normalize_for_cross_env_compare` behavior unchanged.
- `src/snapshot/hook.rs` / `rule.rs` / `schema.rs` — each gains a private `split_*` helper (shared with the existing serializer) and a public `serialize_*_raw`.
- `src/cli/mod.rs` — `Diff` gains `raw: bool`; dispatch forwards it.
- `src/cli/diff.rs` — `raw` threads through `run`, both mode fns, the six local-vs-remote helpers, `normalize_for_diff`, and `canonical_json_for_diff`.
- `tests/cli_diff.rs` — env-vs-env and local-vs-remote raw integration tests.
- `README.md` — one command-table row.

---

## Task 1: Shared `tidy_raw` core in `noise.rs`

**Files:**
- Modify: `src/snapshot/noise.rs`
- Modify: `src/cli/deploy/common.rs:8` (import), remove `sort_string_arrays` def + its 3 tests
- Test: `src/snapshot/noise.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Move `sort_string_arrays` into `noise.rs` and add `tidy_raw`.**

Append to `src/snapshot/noise.rs` (after `sort_keys_recursive`):

```rust
/// Recursively sort every all-string array in the tree alphabetically.
/// Set-like URL arrays (a hook's `queues`, `events`, `run_after`) come
/// back from Rossum in per-env id order; sorting makes cross-env compares
/// order-insensitive. Mixed-type arrays (objects/numbers) are left alone —
/// `content[]` field order is meaningful. (Moved here from
/// `cli::deploy::common` so the snapshot serializers can share it.)
pub(crate) fn sort_string_arrays(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Array(arr) => {
            let all_strings = arr.iter().all(|v| matches!(v, Value::String(_)));
            if all_strings {
                arr.sort_by(|a, b| match (a, b) {
                    (Value::String(s1), Value::String(s2)) => s1.cmp(s2),
                    _ => std::cmp::Ordering::Equal,
                });
            } else {
                for v in arr.iter_mut() {
                    sort_string_arrays(v);
                }
            }
        }
        Value::Object(obj) => {
            for v in obj.values_mut() {
                sort_string_arrays(v);
            }
        }
        _ => {}
    }
}

/// Tidy-raw normalisation for `rdc diff --raw`: sort string-arrays and
/// object keys for readability, but strip NOTHING and rewrite no URLs.
/// The inverse of `normalize_for_cross_env_compare` minus the
/// stripping/rewriting — two payloads compare equal iff they carry the
/// same fields/values modulo key/array ordering.
pub(crate) fn tidy_raw(value: &mut serde_json::Value) {
    sort_string_arrays(value);
    sort_keys_recursive(value);
}
```

- [ ] **Step 2: Delete the old `sort_string_arrays` from `common.rs` and import it.**

In `src/cli/deploy/common.rs`, change the import on line 8 to add `sort_string_arrays`:

```rust
use crate::snapshot::noise::{sort_keys_recursive, sort_string_arrays, strip_noise_fields};
```

Delete the `fn sort_string_arrays(...) { ... }` definition (the whole function, currently ~lines 44-80, including its doc comment) AND delete these three tests from the `mod normalize_tests` block: `sort_string_arrays_sorts_top_level_url_array`, `sort_string_arrays_leaves_mixed_arrays_alone`, `sort_string_arrays_recurses_into_objects`. Leave `normalize_collapses_real_world_email_template_noise` and `normalize_is_key_order_insensitive` in place. The call `sort_string_arrays(&mut value);` inside `normalize_for_cross_env_compare` now resolves to the import.

- [ ] **Step 3: Add unit test for `tidy_raw` in `noise.rs`.**

Add inside the `#[cfg(test)] mod tests` block in `src/snapshot/noise.rs`:

```rust
#[test]
fn tidy_raw_sorts_keys_and_string_arrays_but_strips_nothing() {
    let mut v = serde_json::json!({
        "url": "https://x/api/v1/hooks/1",
        "id": 1,
        "modified_at": "2026-01-01T00:00:00Z",
        "modifier": "https://x/api/v1/users/9",
        "queues": ["https://x/q/3", "https://x/q/1", "https://x/q/2"]
    });
    super::tidy_raw(&mut v);
    // Nothing stripped:
    assert_eq!(v.get("id").and_then(|x| x.as_u64()), Some(1));
    assert!(v.get("url").is_some());
    assert!(v.get("modified_at").is_some());
    assert!(v.get("modifier").is_some());
    // String array sorted:
    assert_eq!(v["queues"], serde_json::json!([
        "https://x/q/1", "https://x/q/2", "https://x/q/3"
    ]));
    // Keys alphabetically sorted (id before url in the serialized form):
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.find("\"id\"").unwrap() < s.find("\"url\"").unwrap());
}
```

- [ ] **Step 4: Build and test.**

Run: `cargo test -p rdc --lib snapshot::noise`
Expected: PASS (new `tidy_raw_*` test green; moved `sort_string_arrays_*` tests, if you also moved them, green).

Run: `cargo test`
Expected: PASS — confirms `common.rs` still compiles against the imported `sort_string_arrays` and all existing normalize tests pass.

- [ ] **Step 5: Commit.**

```bash
git add src/snapshot/noise.rs src/cli/deploy/common.rs
git commit -m "refactor(diff): add tidy_raw, move sort_string_arrays to noise"
```

---

## Task 2: `--raw` flag + env-vs-env raw end-to-end

**Files:**
- Modify: `src/cli/mod.rs:199-204` (flag), `:337` (dispatch)
- Modify: `src/cli/diff.rs:37` (`run`), `:232` (`diff_snapshot_vs_snapshot`), `:319` (`normalize_for_diff`)
- Test: `tests/cli_diff.rs`

- [ ] **Step 1: Add the `--raw` flag to clap and forward it.**

In `src/cli/mod.rs`, the `Diff` variant becomes:

```rust
    Diff {
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        left: String,
        #[arg(add = ArgValueCandidates::new(env_name_candidates))]
        right: Option<String>,
        /// Show the unadjusted diff: reveal id/url/organization, server
        /// back-references, modified_at/modifier, and un-rewritten
        /// cross-reference URLs. Keys and string-arrays stay sorted for
        /// readability.
        #[arg(long)]
        raw: bool,
    },
```

Dispatch (line 337):

```rust
        Some(Command::Diff { left, right, raw }) => crate::cli::diff::run(left, right, raw).await,
```

- [ ] **Step 2: Thread `raw` into `run` and the env-vs-env path.**

In `src/cli/diff.rs`, change `run`'s signature and the env-vs-env call (the local-vs-remote arm stays on its current signature for now — `raw` is consumed by the other arm so it is not unused):

```rust
pub async fn run(left: String, right: Option<String>, raw: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;

    if !cfg.envs.contains_key(&left) {
        return Err(anyhow!("env '{left}' is not defined in rdc.toml"));
    }

    match right {
        None => diff_local_vs_remote(&cwd, &cfg, &left).await,
        Some(other) => {
            if !cfg.envs.contains_key(&other) {
                return Err(anyhow!("env '{other}' is not defined in rdc.toml"));
            }
            diff_snapshot_vs_snapshot(&cwd, &left, &other, raw)
        }
    }
}
```

- [ ] **Step 3: Implement env-vs-env raw in `diff_snapshot_vs_snapshot`.**

Change the signature to `fn diff_snapshot_vs_snapshot(cwd: &Path, src: &str, tgt: &str, raw: bool) -> Result<()>`. Replace the mapping/lockfile/ctx setup block (currently `let mapping_path = ...` through the `let ctx = match (...) {...}`) with this raw-aware version:

```rust
    // In raw mode, skip mapping + lockfile loads entirely — no URL
    // rewriting, so the ctx is always None.
    let mapping_path = src_paths.mapping_file(src, tgt);
    let mapping = if !raw && mapping_path.exists() {
        Mapping::load(&mapping_path).ok()
    } else {
        None
    };
    let src_lockfile = if raw { None } else { Lockfile::load(&src_paths.lockfile()).ok() };
    let tgt_lockfile = if raw { None } else { Lockfile::load(&tgt_paths.lockfile()).ok() };
    let ctx = match (mapping.as_ref(), src_lockfile.as_ref(), tgt_lockfile.as_ref()) {
        (Some(m), Some(s), Some(t)) => Some(RewriteCtx { mapping: m, src_lockfile: s, tgt_lockfile: t }),
        _ => None,
    };
```

Then update the two `normalize_for_diff` calls in the file-loop to pass `raw`:

```rust
                let left_norm = normalize_for_diff(&rel, left, ctx.as_ref(), raw);
                let right_norm = normalize_for_diff(&rel, right, None, raw);
```

- [ ] **Step 4: Add the raw branch to `normalize_for_diff`.**

Change the signature to add `raw: bool` and insert the raw branch right after the non-JSON guard:

```rust
fn normalize_for_diff(rel: &str, bytes: &[u8], rewrite_ctx: Option<&RewriteCtx<'_>>, raw: bool) -> Vec<u8> {
    if !rel.ends_with(".json") {
        return bytes.to_vec();
    }
    if raw {
        if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) {
            crate::snapshot::noise::tidy_raw(&mut value);
            if let Ok(mut pretty) = serde_json::to_vec_pretty(&value) {
                if !pretty.ends_with(b"\n") {
                    pretty.push(b'\n');
                }
                return pretty;
            }
        }
        return bytes.to_vec();
    }
    // ... existing kind-based normalization + rewrite path, unchanged ...
}
```

- [ ] **Step 5: Build to verify it compiles.**

Run: `cargo build`
Expected: success, no warnings about unused `raw`.

- [ ] **Step 6: Add the env-vs-env integration test.**

Append to `tests/cli_diff.rs`:

```rust
#[test]
fn diff_snapshot_vs_snapshot_raw_reveals_id_and_url() {
    // Two hooks differing ONLY in id+url. Normal diff strips them and is
    // silent; --raw must reveal both.
    let project = TempDir::new().unwrap();
    write_two_env_project(project.path());

    let hook_test = serde_json::json!({
        "id": 42,
        "url": "https://test.rossum.app/api/v1/hooks/42",
        "name": "validator-invoices",
        "type": "function",
        "events": ["annotation_status"],
        "queues": [],
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    let hook_prod = serde_json::json!({
        "id": 99,
        "url": "https://prod.rossum.app/api/v1/hooks/99",
        "name": "validator-invoices",
        "type": "function",
        "events": ["annotation_status"],
        "queues": [],
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    std::fs::write(
        project.path().join("envs/test/hooks/validator-invoices.json"),
        serde_json::to_string_pretty(&hook_test).unwrap(),
    ).unwrap();
    std::fs::write(
        project.path().join("envs/prod/hooks/validator-invoices.json"),
        serde_json::to_string_pretty(&hook_prod).unwrap(),
    ).unwrap();

    // Sanity: normal diff is silent.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "test", "prod"])
        .assert().success()
        .stdout(predicate::str::contains("no diffs"));

    // --raw reveals id + both urls.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "test", "prod", "--raw"])
        .assert().success()
        .stdout(predicate::str::contains("\"id\""))
        .stdout(predicate::str::contains("hooks/42"))
        .stdout(predicate::str::contains("hooks/99"))
        .stdout(predicate::str::contains("no diffs").not());
}
```

- [ ] **Step 7: Run the test.**

Run: `cargo test --test cli_diff diff_snapshot_vs_snapshot_raw_reveals_id_and_url`
Expected: PASS.

Run: `cargo test --test cli_diff`
Expected: PASS — existing diff tests unaffected.

- [ ] **Step 8: Commit.**

```bash
git add src/cli/mod.rs src/cli/diff.rs tests/cli_diff.rs
git commit -m "feat(diff): add --raw flag, env-vs-env reveals stripped fields"
```

---

## Task 3: Local-vs-remote raw — hooks

**Files:**
- Modify: `src/snapshot/hook.rs` (refactor `serialize_hook`, add `split_hook_code` + `serialize_hook_raw`)
- Modify: `src/cli/diff.rs:13` (import), `:57` (`diff_local_vs_remote`), `:117`+`:174-175` (`diff_hooks`)
- Test: `src/snapshot/hook.rs`, `tests/cli_diff.rs`

- [ ] **Step 1: Extract `split_hook_code` and add `serialize_hook_raw`.**

In `src/snapshot/hook.rs`, add the helper and the raw serializer (next to `serialize_hook`):

```rust
/// Remove `config.code` (a string) from a serialized hook Value and return
/// it for the sidecar. Shared by `serialize_hook` and `serialize_hook_raw`.
fn split_hook_code(json_value: &mut Value) -> Option<String> {
    json_value
        .get_mut("config")
        .and_then(|c| c.as_object_mut())
        .and_then(|m| m.remove("code"))
        .and_then(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        })
}

/// Like [`serialize_hook`] but for `rdc diff --raw`: splits code to the
/// sidecar and tidies (sort keys + string-arrays), but does NOT strip
/// `modified_at` and does NOT apply the curated `HOOK_KEY_ORDER`. Reveals
/// the server-managed fields a normal diff hides.
pub fn serialize_hook_raw(hook: &Hook) -> Result<(Vec<u8>, Option<String>)> {
    let mut json_value = serde_json::to_value(hook)
        .context("serializing hook to value")?;
    let code = split_hook_code(&mut json_value);
    crate::snapshot::noise::tidy_raw(&mut json_value);
    let mut bytes = serde_json::to_vec_pretty(&json_value)
        .context("serializing hook json")?;
    bytes.push(b'\n');
    Ok((bytes, code))
}
```

Then refactor `serialize_hook` to reuse the helper — replace its code-extraction block (the `let code = json_value.get_mut("config")...;` expression, currently ~lines 84-91) with:

```rust
    let code = split_hook_code(&mut json_value);
```

- [ ] **Step 2: Add a unit test contrasting raw vs canonical.**

Add inside the `#[cfg(test)] mod tests` in `src/snapshot/hook.rs`:

```rust
#[test]
fn serialize_hook_raw_keeps_modified_at_and_splits_code() {
    let h: Hook = serde_json::from_value(serde_json::json!({
        "id": 1,
        "url": "https://x/api/v1/hooks/1",
        "name": "h",
        "type": "function",
        "queues": [],
        "events": ["annotation_status"],
        "modified_at": "2026-01-01T00:00:00Z",
        "config": { "runtime": "python3.12", "code": "pass\n" }
    })).unwrap();

    let (json, code) = serialize_hook_raw(&h).unwrap();
    let s = String::from_utf8(json).unwrap();
    assert!(s.contains("modified_at"), "raw must retain modified_at: {s}");
    assert!(!s.contains("\"code\""), "code must be split to the sidecar");
    assert_eq!(code.as_deref(), Some("pass\n"));

    // Contrast: the canonical serializer strips modified_at.
    let (canon, _) = serialize_hook(&h).unwrap();
    assert!(!String::from_utf8(canon).unwrap().contains("modified_at"));
}
```

- [ ] **Step 3: Run the unit test.**

Run: `cargo test -p rdc --lib snapshot::hook::tests::serialize_hook_raw_keeps_modified_at_and_splits_code`
Expected: PASS.

- [ ] **Step 4: Wire `raw` into `diff_local_vs_remote` and `diff_hooks`.**

In `src/cli/diff.rs`, extend the import on line 13:

```rust
use crate::snapshot::hook::{read_hook, serialize_hook, serialize_hook_raw};
```

Change `diff_local_vs_remote`'s signature to `pub async fn diff_local_vs_remote(cwd: &Path, cfg: &ProjectConfig, env: &str, raw: bool) -> Result<()>`, and update its `diff_hooks` call to pass `raw` (leave the other helper calls — `diff_rules`, `diff_labels`, `diff_engines`, `diff_engine_fields`, `diff_queue_tree` — unchanged for now):

```rust
        diff_hooks(&paths, &lockfile, &client, &mut diffs_printed, &progress, raw).await?;
```

Update `run`'s local-vs-remote arm to pass `raw`:

```rust
        None => diff_local_vs_remote(&cwd, &cfg, &left, raw).await,
```

Change `diff_hooks`'s signature to add `raw: bool` (append after `progress: &Arc<Log>`), and replace the two serialize calls (lines 174-175) with:

```rust
        let (local_json, local_code) =
            if raw { serialize_hook_raw(&local)? } else { serialize_hook(&local)? };
        let (remote_json, remote_code) =
            if raw { serialize_hook_raw(&remote)? } else { serialize_hook(&remote)? };
```

- [ ] **Step 5: Build.**

Run: `cargo build`
Expected: success (no unused-`raw` warnings — `diff_local_vs_remote` uses it via `diff_hooks`).

- [ ] **Step 6: Add the local-vs-remote raw integration test.**

Append to `tests/cli_diff.rs`:

```rust
#[tokio::test]
async fn diff_local_remote_raw_reveals_modified_at() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server).await;

    // A single hook carrying modified_at (stripped from the local snapshot
    // on pull, present on the live remote).
    let hook_body = serde_json::json!({
        "id": 1,
        "url": format!("{}/api/v1/hooks/1", server.uri()),
        "name": "validator-invoices",
        "type": "function",
        "queues": [],
        "events": ["annotation_status"],
        "modified_at": "2026-04-01T10:00:00Z",
        "config": { "runtime": "python3.12", "code": "pass\n" }
    });
    Mock::given(method("GET")).and(path("/api/v1/hooks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pagination": { "next": null },
            "results": [hook_body.clone()]
        })))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/api/v1/hooks/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hook_body.clone()))
        .mount(&server).await;
    for ep in [
        "/api/v1/workspaces", "/api/v1/queues", "/api/v1/rules", "/api/v1/labels",
        "/api/v1/engines", "/api/v1/engine_fields", "/api/v1/workflows",
        "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET")).and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_list()))
            .mount(&server).await;
    }

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push"])
        .assert().success();

    // Normal diff: modified_at stripped from both sides → silent.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("no diffs"));

    // --raw: remote carries modified_at, local snapshot dropped it → shown.
    Command::cargo_bin("rdc").unwrap()
        .current_dir(project.path())
        .args(["diff", "dev", "--raw"])
        .assert().success()
        .stdout(predicate::str::contains("modified_at"));
}
```

- [ ] **Step 7: Run the test.**

Run: `cargo test --test cli_diff diff_local_remote_raw_reveals_modified_at`
Expected: PASS.

- [ ] **Step 8: Commit.**

```bash
git add src/snapshot/hook.rs src/cli/diff.rs tests/cli_diff.rs
git commit -m "feat(diff): --raw reveals hook modified_at in local-vs-remote"
```

---

## Task 4: Local-vs-remote raw — rules and schemas

**Files:**
- Modify: `src/snapshot/rule.rs` (refactor `serialize_rule`, add `split_rule_trigger_condition` + `serialize_rule_raw`)
- Modify: `src/snapshot/schema.rs` (refactor `serialize_schema`, add `split_schema_formulas` + `serialize_schema_raw`)
- Modify: `src/cli/diff.rs` (`diff_rules`, schema branch of `diff_queue_tree`, imports, `diff_local_vs_remote` calls)
- Test: `src/snapshot/rule.rs`, `src/snapshot/schema.rs`

- [ ] **Step 1: Add `split_rule_trigger_condition` + `serialize_rule_raw`.**

In `src/snapshot/rule.rs`:

```rust
/// Remove a string `trigger_condition` from a serialized rule Value and
/// return it for the sidecar. Shared by `serialize_rule`/`serialize_rule_raw`.
fn split_rule_trigger_condition(json_value: &mut Value) -> Option<String> {
    if matches!(json_value.get("trigger_condition"), Some(Value::String(_))) {
        json_value
            .as_object_mut()
            .and_then(|m| m.remove("trigger_condition"))
            .and_then(|v| if let Value::String(s) = v { Some(s) } else { None })
    } else {
        None
    }
}

/// Like [`serialize_rule`] but for `rdc diff --raw`: splits
/// `trigger_condition` to the sidecar and tidies, without stripping
/// server-managed fields.
pub fn serialize_rule_raw(r: &Rule) -> Result<(Vec<u8>, Option<String>)> {
    let mut json_value = serde_json::to_value(r).context("serializing rule to value")?;
    let code = split_rule_trigger_condition(&mut json_value);
    crate::snapshot::noise::tidy_raw(&mut json_value);
    let mut bytes = serde_json::to_vec_pretty(&json_value).context("serializing rule json")?;
    bytes.push(b'\n');
    Ok((bytes, code))
}
```

Refactor `serialize_rule` to replace its `let code = if matches!(...) {...} else { None };` block with:

```rust
    let code = split_rule_trigger_condition(&mut json_value);
```

- [ ] **Step 2: Add `split_schema_formulas` + `serialize_schema_raw`.**

In `src/snapshot/schema.rs`:

```rust
/// Walk schema `content[]`, extract every datapoint `formula` (removing it
/// from the JSON), and return `(field_id, code_bytes)` sorted by id.
/// Shared by `serialize_schema`/`serialize_schema_raw`.
fn split_schema_formulas(value: &mut Value) -> Vec<(String, Vec<u8>)> {
    let mut formulas: Vec<(String, String)> = Vec::new();
    if let Some(content) = value.get_mut("content").and_then(|c| c.as_array_mut()) {
        for node in content.iter_mut() {
            extract_formulas(node, &mut formulas);
        }
    }
    formulas.sort_by(|a, b| a.0.cmp(&b.0));
    formulas.into_iter().map(|(id, code)| (id, code.into_bytes())).collect()
}

/// Like [`serialize_schema`] but for `rdc diff --raw`: splits formulas to
/// sidecars and tidies, without stripping server-managed fields.
pub fn serialize_schema_raw(schema: &Schema) -> Result<(Vec<u8>, Vec<(String, Vec<u8>)>)> {
    let mut value = serde_json::to_value(schema).context("serializing schema to value")?;
    let formulas = split_schema_formulas(&mut value);
    crate::snapshot::noise::tidy_raw(&mut value);
    let mut bytes = serde_json::to_vec_pretty(&value).context("serializing schema json")?;
    bytes.push(b'\n');
    Ok((bytes, formulas))
}
```

Refactor `serialize_schema` to replace its formula-extraction block (the `let mut formulas...` through `formulas.sort_by(...)` and the final `formulas_bytes` mapping) so it uses the helper:

```rust
    let formulas = split_schema_formulas(&mut value);

    crate::snapshot::key_order::strip_hidden_fields_recursive(&mut value);

    let mut bytes = serde_json::to_vec_pretty(&value)
        .context("serializing schema json")?;
    bytes.push(b'\n');

    Ok((bytes, formulas))
```

- [ ] **Step 3: Add unit tests for both raw serializers.**

In `src/snapshot/rule.rs` `#[cfg(test)]`:

```rust
#[test]
fn serialize_rule_raw_keeps_modified_at_and_splits_trigger() {
    let r: Rule = serde_json::from_value(serde_json::json!({
        "id": 5,
        "url": "https://x/api/v1/rules/5",
        "name": "r",
        "trigger_condition": "field.x > 0",
        "modified_at": "2026-01-01T00:00:00Z"
    })).unwrap();
    let (json, code) = serialize_rule_raw(&r).unwrap();
    let s = String::from_utf8(json).unwrap();
    assert!(s.contains("modified_at"));
    assert!(!s.contains("trigger_condition"));
    assert_eq!(code.as_deref(), Some("field.x > 0"));
}
```

In `src/snapshot/schema.rs` `#[cfg(test)]`:

```rust
#[test]
fn serialize_schema_raw_keeps_modified_at() {
    let s: Schema = serde_json::from_value(serde_json::json!({
        "id": 7,
        "url": "https://x/api/v1/schemas/7",
        "name": "s",
        "content": [],
        "modified_at": "2026-01-01T00:00:00Z"
    })).unwrap();
    let (json, _formulas) = serialize_schema_raw(&s).unwrap();
    assert!(String::from_utf8(json).unwrap().contains("modified_at"));
}
```

- [ ] **Step 4: Run unit tests.**

Run: `cargo test -p rdc --lib snapshot::rule snapshot::schema`
Expected: PASS (new raw tests + existing serializer tests green after the refactor).

- [ ] **Step 5: Wire `raw` into `diff_rules` and the schema branch of `diff_queue_tree`.**

In `src/cli/diff.rs`:

- Line 16 import: `use crate::snapshot::schema::{read_schema, serialize_schema, serialize_schema_raw};`
- In `diff_rules`, line 561 import: `use crate::snapshot::rule::{read_rule, serialize_rule, serialize_rule_raw};`
- Change `diff_rules` signature to add `raw: bool`; replace its serialize calls:

```rust
        let (local_json, local_code) =
            if raw { serialize_rule_raw(&local)? } else { serialize_rule(&local)? };
```
```rust
        let (remote_json, remote_code) =
            if raw { serialize_rule_raw(remote)? } else { serialize_rule(remote)? };
```

- Change `diff_queue_tree` signature to add `raw: bool`; replace its schema serialize calls (lines 958-959):

```rust
        let (lj, l_formulas) =
            if raw { serialize_schema_raw(&local)? } else { serialize_schema(&local)? };
        let (rj, r_formulas) =
            if raw { serialize_schema_raw(&remote)? } else { serialize_schema(&remote)? };
```

- In `diff_local_vs_remote`, update the two calls to pass `raw`:

```rust
    diff_rules(&paths, &lockfile, &client, &mut diffs_printed, &progress, raw).await?;
```
```rust
        diff_queue_tree(&paths, &lockfile, &client, &mut diffs_printed, &progress, raw).await?;
```

(The `canonical_json_for_diff` calls inside `diff_queue_tree` for queues/inboxes/email-templates stay on the current signature — wired in Task 5. `raw` is already used by the schema branch, so no unused warning.)

- [ ] **Step 6: Build and run diff tests.**

Run: `cargo build`
Expected: success.

Run: `cargo test --test cli_diff`
Expected: PASS — no regressions.

- [ ] **Step 7: Commit.**

```bash
git add src/snapshot/rule.rs src/snapshot/schema.rs src/cli/diff.rs
git commit -m "feat(diff): --raw raw serializers for rules and schemas"
```

---

## Task 5: Local-vs-remote raw — flat kinds

**Files:**
- Modify: `src/cli/diff.rs` (`canonical_json_for_diff`, `diff_labels`/`diff_flat_remote`, `diff_engines`, `diff_engine_fields`, remaining `diff_queue_tree` calls, `diff_local_vs_remote` calls)
- Test: `src/cli/diff.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Add a `raw` param to `canonical_json_for_diff`.**

In `src/cli/diff.rs`:

```rust
fn canonical_json_for_diff<T: serde::Serialize>(v: &T, raw: bool) -> Result<String> {
    if raw {
        let mut value = serde_json::to_value(v).context("serializing for raw diff")?;
        crate::snapshot::noise::tidy_raw(&mut value);
        let mut bytes = serde_json::to_vec_pretty(&value)?;
        bytes.push(b'\n');
        return Ok(String::from_utf8(bytes)?);
    }
    let mut bytes = serde_json::to_vec_pretty(v)?;
    bytes.push(b'\n');
    Ok(String::from_utf8(bytes)?)
}
```

- [ ] **Step 2: Thread `raw` into every `canonical_json_for_diff` caller.**

Add `raw: bool` to the signatures of `diff_labels`, `diff_engines`, `diff_engine_fields`, and `diff_flat_remote`, and update each `canonical_json_for_diff(x)?` call to `canonical_json_for_diff(x, raw)?`. Concretely:

- `diff_engines`: both calls (currently lines 653, 659) → `canonical_json_for_diff(&local, raw)?` and `canonical_json_for_diff(remote, raw)?`.
- `diff_engine_fields`: both calls (714, 720) → pass `raw`.
- `diff_flat_remote<T>`: add `raw: bool` param; both calls (777, 786) → pass `raw`.
- `diff_labels`: add `raw: bool`; its `diff_flat_remote(...)` call passes `raw` as the new trailing arg.
- `diff_queue_tree`: the queue (936-937), inbox (980-981), and email-template (1001-1002) `canonical_json_for_diff(...)` calls → pass `raw`. (`raw` is already a param from Task 4.)

In `diff_local_vs_remote`, pass `raw` to the remaining helper calls:

```rust
    diff_labels(&paths, &lockfile, &client, &mut diffs_printed, &progress, raw).await?;
    diff_engines(&paths, &lockfile, &client, &mut diffs_printed, &progress, raw).await?;
    diff_engine_fields(&paths, &lockfile, &client, &mut diffs_printed, &progress, raw).await?;
```

- [ ] **Step 3: Add a unit test for `canonical_json_for_diff` raw.**

Add a test module at the bottom of `src/cli/diff.rs`:

```rust
#[cfg(test)]
mod raw_tests {
    use super::canonical_json_for_diff;

    #[derive(serde::Serialize)]
    struct Dummy { url: String, id: u64, modified_at: String }

    #[test]
    fn canonical_json_for_diff_raw_sorts_keys_and_keeps_fields() {
        let d = Dummy { url: "u".into(), id: 1, modified_at: "t".into() };
        let raw = canonical_json_for_diff(&d, true).unwrap();
        // Alphabetical key order: id < modified_at < url.
        let id = raw.find("\"id\"").unwrap();
        let m = raw.find("\"modified_at\"").unwrap();
        let u = raw.find("\"url\"").unwrap();
        assert!(id < m && m < u, "raw output must be key-sorted: {raw}");
        assert!(raw.contains("modified_at"));
    }
}
```

- [ ] **Step 4: Build, lint, and test.**

Run: `cargo build`
Expected: success (every `canonical_json_for_diff` call now passes `raw`; no unused params).

Run: `cargo test -p rdc --lib cli::diff::raw_tests`
Expected: PASS.

Run: `cargo test --test cli_diff`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add src/cli/diff.rs
git commit -m "feat(diff): --raw covers flat kinds and queue-tree sub-objects"
```

---

## Task 6: Docs and full regression

**Files:**
- Modify: `README.md:213-214`
- (Help text already added in Task 2.)

- [ ] **Step 1: Add a README command-table row.**

In `README.md`, after the two `rdc diff` rows (lines 213-214), add:

```markdown
| `rdc diff … --raw` | Unadjusted diff: reveal id/url/organization, modified_at/modifier, server back-references, and un-rewritten cross-reference URLs. Keys/arrays stay sorted. |
```

- [ ] **Step 2: Full regression suite.**

Run: `cargo test`
Expected: PASS (whole suite).

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

Run: `cargo fmt --check`
Expected: clean (run `cargo fmt` and restage if it reports diffs).

- [ ] **Step 3: Manual smoke check (optional but recommended).**

In a real project dir with two synced envs:
```bash
rdc diff <a> <b>          # normal: env-specific noise hidden
rdc diff <a> <b> --raw    # reveals id/url/organization/back-refs
```
Confirm `--raw` shows the extra fields and normal mode stays quiet.

- [ ] **Step 4: Commit.**

```bash
git add README.md
git commit -m "docs(diff): document --raw flag"
```

---

## Self-Review

**Spec coverage:**
- Single `--raw` flag, both modes → Tasks 2 (flag + env-vs-env), 3-5 (local-vs-remote). ✓
- "Reveal, keep tidy" (no strip/rewrite; keep sort + sidecar) → `tidy_raw` (Task 1) + raw branch/serializers (Tasks 2-5). ✓
- Shared core → `tidy_raw` in `noise.rs`, reused everywhere. ✓
- Zero change when flag absent → every change is behind `if raw`; regression gates in Tasks 2-6. ✓
- Overlay limitation → documented in spec; no code path claims otherwise. ✓
- Affected files & tests → match the spec's lists. ✓

**Placeholder scan:** No TBD/TODO; every code step shows full code; every test step shows the test and the run command + expected result.

**Type/signature consistency:** `tidy_raw(&mut serde_json::Value)`, `serialize_hook_raw`/`serialize_rule_raw` → `(Vec<u8>, Option<String>)`, `serialize_schema_raw` → `(Vec<u8>, Vec<(String, Vec<u8>)>)`, `canonical_json_for_diff(&T, bool)`, and the `raw: bool` trailing param on `run`, `diff_local_vs_remote`, `diff_snapshot_vs_snapshot`, `normalize_for_diff`, and all six `diff_*` helpers — consistent across tasks. Incremental wiring keeps every task compiling (each function that gains `raw` uses it in the same task).
