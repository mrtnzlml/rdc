# Manual MDH Datasets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an MDH dataset be declared *manual* so its row data is pulled into the snapshot, versioned in git, and replicated to other environments by the existing `rdc migrate` + `rdc sync` workflow.

**Architecture:** A dataset opts in with `"data": "manual"` in its `collection.json`. Its rows live in `envs/<env>/mdh/<slug>/data.jsonl` in a strictly canonical form (compact JSON per line, keys sorted recursively, lines sorted byte-wise, server-generated `$oid` `_id`s stripped). Pull reuses rdc's existing byte-generic three-way machinery under a new `mdh_data` lockfile kind. Push makes the env match the file with a hybrid keyed diff — `replace_one` in place for rows carrying a business key, content-addressed multiset insert/delete for the rest, deletes first, deletions behind the existing delete gate. Migrate carries the file verbatim once `jsonl` is a managed leaf.

**Tech Stack:** Rust 2024, `anyhow`, `serde_json` (with `preserve_order`, without `arbitrary_precision`), `reqwest`, `futures`, `wiremock` + `tokio` + `pretty_assertions` for tests.

**Spec:** `docs/superpowers/specs/2026-08-18-manual-mdh-datasets-design.md` — read it first. Every `A<n>` / `C<n>` reference below cites a verified fact in that document's "Verified facts" tables.

## Global Constraints

- **Customer confidentiality (CLAUDE.md):** never put customer names or customer-specific identifiers (org/division/region codes, real env names, queue/engine/hook slugs, hostnames, URLs, file paths) in source, tests, docs, fixtures, **or commit messages**. Use neutral placeholders: `acme`, `main`, `invoices`, `gl-codes`, `GL_CODES`, `synonyms`, `dev`/`test`/`prod`.
- **Never run repo-wide `cargo fmt`.** This tree is not fmt-clean under the local rustfmt; a pre-existing `cargo fmt --check` failure is not a regression. Match surrounding style by hand.
- **`dead_code = "deny"` is on** (`Cargo.toml`, `[workspace.lints.rust]`). Every non-`pub` item you add must be used within the same commit, or the build fails. `pub` methods on the `pub` `DataStorageClient` count as used (public lib API).
- **Only the synchronous Data Storage verbs** (`data/find`, `data/insert_many`, `data/delete_many`, `data/replace_one`, `data/aggregate`). Never `data/bulk_write`: its 202 carries an empty `message`, so completion is unobservable (A8).
- **Row-count ceiling:** warn above 1 000 rows, hard error above 10 000, in both directions (D4).
- **`replace_one` replacements always omit `_id`** — including it risks the immutable-`_id` rejection (A15).
- Commit after every task. Work directly on `main`; **never `git push`**.
- Full suite before the final commit: `cargo test`.

---

### Task 1: Canonical `data.jsonl` form

The on-disk form must be byte-deterministic because a non-JSON file is hashed verbatim (C1) — without one canonical form, pull and push rewrite it forever.

**Files:**
- Create: `src/snapshot/mdh_data.rs`
- Modify: `src/snapshot/mod.rs:8` (add `pub mod mdh_data;` in alphabetical order, after `index_set`)
- Test: inline `mod tests` in `src/snapshot/mdh_data.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn is_oid(v: &Value) -> bool`
  - `pub fn canonicalize_row(row: &Value) -> Value`
  - `pub fn to_jsonl(rows: &[Value]) -> anyhow::Result<Vec<u8>>`
  - `pub fn from_jsonl(bytes: &[u8], display_path: &str) -> anyhow::Result<Vec<Value>>`
  - `pub const ROW_WARN_THRESHOLD: usize = 1_000;`
  - `pub const ROW_HARD_LIMIT: usize = 10_000;`
  - `pub const DATA_FILE: &str = "data.jsonl";`

- [ ] **Step 1: Write the failing tests**

Create `src/snapshot/mdh_data.rs` containing ONLY the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    /// The `$oid` wrapper is the live-verified marker of a server-generated
    /// ObjectId (A2) — the one `_id` shape that is safe to strip.
    #[test]
    fn is_oid_matches_only_the_single_key_wrapper() {
        assert!(is_oid(&json!({ "$oid": "6a8403a6070b60eaa348d173" })));
        assert!(!is_oid(&json!("natural-key-1")));
        assert!(!is_oid(&json!(42)));
        assert!(!is_oid(&json!({ "$oid": "x", "other": 1 })));
        assert!(!is_oid(&json!({})));
    }

    #[test]
    fn canonicalize_sorts_keys_recursively_and_leaves_arrays_ordered() {
        let row = json!({
            "zeta": 1,
            "alpha": { "b": 2, "a": { "d": 4, "c": 3 } },
            "arr": [ { "y": 1, "x": 2 }, "second", "first" ]
        });
        // Arrays keep their element order (order is data); only object keys sort.
        assert_eq!(
            serde_json::to_string(&canonicalize_row(&row)).unwrap(),
            r#"{"alpha":{"a":{"c":3,"d":4},"b":2},"arr":[{"x":2,"y":1},"second","first"],"zeta":1}"#
        );
    }

    #[test]
    fn canonicalize_drops_server_id_but_keeps_business_key() {
        let server = json!({ "_id": { "$oid": "6a8403a6070b60eaa348d173" }, "code": "1000" });
        assert_eq!(canonicalize_row(&server), json!({ "code": "1000" }));

        let business = json!({ "_id": "gl-1000", "code": "1000" });
        assert_eq!(canonicalize_row(&business), business);

        let numeric = json!({ "_id": 7, "code": "1000" });
        assert_eq!(canonicalize_row(&numeric), numeric);
    }

    #[test]
    fn to_jsonl_is_line_sorted_compact_and_newline_terminated() {
        let rows = vec![
            json!({ "code": "2000", "label": "Travel" }),
            json!({ "code": "1000", "label": "Office supplies" }),
        ];
        let out = to_jsonl(&rows).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"code\":\"1000\",\"label\":\"Office supplies\"}\n\
             {\"code\":\"2000\",\"label\":\"Travel\"}\n"
        );
    }

    #[test]
    fn to_jsonl_of_no_rows_is_empty_not_a_blank_line() {
        // A manual dataset with zero rows is a 0-byte file — that is what
        // distinguishes "manual and empty" from "not manual" (no file).
        assert!(to_jsonl(&[]).unwrap().is_empty());
    }

    /// The whole point of a canonical form: feeding it back through the
    /// pipeline must be a fixed point, or every sync rewrites the file (C1).
    #[test]
    fn jsonl_round_trip_is_idempotent_across_the_verified_type_matrix() {
        // Exactly the types verified to round-trip byte-identically through the
        // Data Storage API (A4).
        let rows = vec![json!({
            "_id": "natural-key-1",
            "int": 42,
            "long": 9007199254740993i64,
            "float": 3.14,
            "bool": true,
            "nil": null,
            "uni": "Přílöhá — ünïcode",
            "trailws": "sep ",
            "empty_obj": {},
            "empty_arr": [],
            "arr": [1, "two", { "three": 3 }],
            "nested": { "a": { "b": "c" } },
            "ejson_date": { "$date": "2026-01-15T00:00:00Z" },
            "dotted.key": "dotted",
            "dollar_in_value": "$gt"
        })];
        let first = to_jsonl(&rows).unwrap();
        let parsed = from_jsonl(&first, "data.jsonl").unwrap();
        let second = to_jsonl(&parsed).unwrap();
        assert_eq!(first, second, "canonical form must be a fixed point");
        assert_eq!(parsed[0]["long"], json!(9007199254740993i64));
        assert_eq!(parsed[0]["trailws"], json!("sep "), "trailing ws is data");
    }

    /// A hand-written `$oid` `_id` must be dropped on READ too. Without that
    /// symmetry the two sides could never compare equal and the dataset would
    /// churn forever (spec, on-disk rule 5).
    #[test]
    fn from_jsonl_drops_a_hand_written_server_id() {
        let line = b"{\"_id\":{\"$oid\":\"6a8403a6070b60eaa348d173\"},\"code\":\"1000\"}\n";
        assert_eq!(from_jsonl(line, "data.jsonl").unwrap(), vec![json!({ "code": "1000" })]);
    }

    #[test]
    fn from_jsonl_skips_blank_lines_and_reports_the_offending_line_number() {
        let ok = b"{\"a\":1}\n\n{\"b\":2}\n";
        assert_eq!(from_jsonl(ok, "data.jsonl").unwrap().len(), 2);

        let bad = b"{\"a\":1}\nnot json\n";
        let err = format!("{:#}", from_jsonl(bad, "envs/dev/mdh/gl-codes/data.jsonl").unwrap_err());
        assert!(err.contains("data.jsonl:2"), "must name the line: {err}");

        let scalar = b"\"just a string\"\n";
        let err = format!("{:#}", from_jsonl(scalar, "data.jsonl").unwrap_err());
        assert!(err.contains("data.jsonl:1"), "must name the line: {err}");
        assert!(err.contains("object"), "must say what was wrong: {err}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib snapshot::mdh_data`
Expected: compile error — `cannot find function is_oid` (and the module is not declared yet).

- [ ] **Step 3: Declare the module**

In `src/snapshot/mod.rs`, insert after line 8 (`pub mod index_set;`), keeping alphabetical order:

```rust
pub mod mdh_data;
```

- [ ] **Step 4: Write the implementation**

Prepend to `src/snapshot/mdh_data.rs`, above the test module:

```rust
//! Canonical on-disk form for MDH row data (`envs/<env>/mdh/<slug>/data.jsonl`).
//!
//! `data.jsonl` is not JSON, so `canonicalize_for_hash` hashes it VERBATIM
//! (`snapshot/noise.rs:57-60`). Every byte therefore has to be canonical or
//! pull and push would rewrite the file on every cycle. The canonical form:
//!
//! 1. one JSON object per line, compact (no insignificant whitespace);
//! 2. object keys sorted lexicographically, recursively (array element order
//!    is data and is preserved);
//! 3. lines sorted by their canonical bytes;
//! 4. LF endings, trailing newline; zero rows is a 0-byte file;
//! 5. `_id` dropped if and only if it is the EJSON `{"$oid": …}` wrapper —
//!    a server-generated ObjectId (live-verified marker). Any other `_id`
//!    (string, int, …) is a business key the user authored and is preserved.
//!
//! Rule 5 is applied to BOTH directions — rows read from the env and rows read
//! from disk — so a hand-written `$oid` `_id` is ignored for comparison and
//! removed the next time the file is written. Without that symmetry the two
//! sides could never compare equal.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

/// On-disk filename for a manual dataset's rows.
pub const DATA_FILE: &str = "data.jsonl";

/// Row count above which rdc warns that a dataset is large for git.
pub const ROW_WARN_THRESHOLD: usize = 1_000;

/// Row count above which rdc refuses to version a dataset at all.
pub const ROW_HARD_LIMIT: usize = 10_000;

/// True when `v` is the EJSON wrapper for a server-generated ObjectId: an
/// object whose single key is `$oid`.
pub fn is_oid(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == 1 && o.contains_key("$oid"))
}

/// Reduce a row to its canonical form: keys sorted recursively, a
/// server-generated `_id` dropped.
pub fn canonicalize_row(row: &Value) -> Value {
    let mut out = sort_keys(row);
    if let Value::Object(obj) = &mut out
        && obj.get("_id").is_some_and(is_oid)
    {
        obj.shift_remove("_id");
    }
    out
}

/// Recursively sort object keys. Array ORDER is data and is left alone; only
/// the objects inside are rewritten.
fn sort_keys(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let mut m = Map::new();
            for k in keys {
                m.insert(k.clone(), sort_keys(&o[k]));
            }
            Value::Object(m)
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// Serialize rows to the canonical `data.jsonl` bytes.
pub fn to_jsonl(rows: &[Value]) -> Result<Vec<u8>> {
    let mut lines: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
    for row in rows {
        let canonical = canonicalize_row(row);
        if !canonical.is_object() {
            bail!("MDH row is not a JSON object: {canonical}");
        }
        lines.push(serde_json::to_vec(&canonical).context("serializing an MDH row")?);
    }
    lines.sort();
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(&line);
        out.push(b'\n');
    }
    Ok(out)
}

/// Parse canonical `data.jsonl` bytes back into rows, canonicalizing each one.
/// `display_path` appears in error messages (`<path>:<line>`), so a bad hand
/// edit points at the exact line.
pub fn from_jsonl(bytes: &[u8], display_path: &str) -> Result<Vec<Value>> {
    let text = std::str::from_utf8(bytes)
        .with_context(|| format!("{display_path} is not valid UTF-8"))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .with_context(|| format!("{display_path}:{} is not valid JSON", i + 1))?;
        if !value.is_object() {
            bail!("{display_path}:{} is not a JSON object", i + 1);
        }
        out.push(canonicalize_row(&value));
    }
    Ok(out)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib snapshot::mdh_data`
Expected: 8 passed.

- [ ] **Step 6: Commit**

```bash
git add src/snapshot/mdh_data.rs src/snapshot/mod.rs
git commit -m "feat(mdh): canonical data.jsonl form for MDH row data"
```

---

### Task 2: `data.jsonl` path helper

**Files:**
- Modify: `src/paths.rs:263-266` (add a method after `dataset_dir`)
- Test: inline `mod tests` in `src/paths.rs` (beside `dataset_dir_path` at :470)

**Interfaces:**
- Consumes: `snapshot::mdh_data::DATA_FILE` (Task 1).
- Produces: `pub fn dataset_data(&self, dataset_slug: &str) -> PathBuf`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` in `src/paths.rs`, right after `fn dataset_dir_path()`:

```rust
    #[test]
    fn dataset_data_path() {
        assert_eq!(
            p().dataset_data("gl-codes"),
            Path::new("/proj/envs/dev/mdh/gl-codes/data.jsonl")
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib paths::tests::dataset_data_path`
Expected: compile error — `no method named dataset_data`.

- [ ] **Step 3: Implement**

In `src/paths.rs`, immediately after the `dataset_dir` method:

```rust
    /// `<root>/envs/<env>/mdh/<dataset_slug>/data.jsonl` — the row data of a
    /// dataset flagged `"data": "manual"`. Absent for every other dataset.
    pub fn dataset_data(&self, dataset_slug: &str) -> PathBuf {
        self.dataset_dir(dataset_slug)
            .join(crate::snapshot::mdh_data::DATA_FILE)
    }
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test --lib paths::tests::dataset_data_path`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/paths.rs
git commit -m "feat(mdh): add Paths::dataset_data for the row-data file"
```

---

### Task 3: Manifest merge + the `data: manual` flag

**This task fixes a latent blocker (C4):** `pull::mdh::process` rewrites `collection.json` from server truth whenever the bytes differ, so a hand-added flag would be erased on the next pull.

**Files:**
- Modify: `src/cli/pull/mdh.rs:20-38` (replace `collection_manifest_bytes`, add `DataMode` + `read_data_mode`), `src/cli/pull/mdh.rs:500-509` (the manifest write in sub-phase A)
- Test: inline `mod tests` in `src/cli/pull/mdh.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub(crate) enum DataMode { None, Manual }` (derives `Debug, Clone, Copy, PartialEq, Eq`)
  - `pub(crate) fn read_data_mode(dataset_dir: &Path) -> anyhow::Result<DataMode>`
  - `pub(crate) fn manifest_bytes_merged(existing: Option<&[u8]>, name: &str) -> anyhow::Result<Vec<u8>>`
  - `pub(crate) const MANUAL_DATA_VALUE: &str = "manual";`

Note: `collection_manifest_bytes(name)` is REPLACED by `manifest_bytes_merged`. Its existing call sites are `pull/mdh.rs:501` and the tests at `:651`; `execute.rs` does not call it. Grep to confirm before deleting: `grep -rn collection_manifest_bytes src/ tests/`.

- [ ] **Step 1: Write the failing tests**

Replace the existing `collection_manifest_round_trips_name` test in `src/cli/pull/mdh.rs` with:

```rust
    #[test]
    fn manifest_bytes_for_a_fresh_dataset_are_unchanged_from_the_legacy_form() {
        // Byte-identity with the pre-merge writer: an existing project must not
        // see churn in its collection.json files.
        assert_eq!(
            manifest_bytes_merged(None, "GL_CODES").unwrap(),
            b"{\n  \"name\": \"GL_CODES\"\n}\n"
        );
    }

    #[test]
    fn manifest_merge_preserves_the_manual_flag_while_refreshing_name() {
        // The pull rewrites `name` from server truth; every OTHER key the user
        // added must survive, or the opt-in flag would be erased on next pull.
        let existing = b"{\n  \"name\": \"OLD_NAME\",\n  \"data\": \"manual\"\n}\n";
        let merged = manifest_bytes_merged(Some(existing), "GL_CODES").unwrap();
        let v: Value = serde_json::from_slice(&merged).unwrap();
        assert_eq!(v["name"], serde_json::json!("GL_CODES"));
        assert_eq!(v["data"], serde_json::json!("manual"));
        assert_eq!(merged.last(), Some(&b'\n'), "trailing newline required");
    }

    #[test]
    fn manifest_merge_is_idempotent() {
        let once = manifest_bytes_merged(
            Some(b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"manual\"\n}\n"),
            "GL_CODES",
        )
        .unwrap();
        let twice = manifest_bytes_merged(Some(&once), "GL_CODES").unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn manifest_merge_tolerates_unparseable_existing_bytes() {
        // A corrupt manifest must not wedge the pull: fall back to a fresh one.
        assert_eq!(
            manifest_bytes_merged(Some(b"not json"), "GL_CODES").unwrap(),
            b"{\n  \"name\": \"GL_CODES\"\n}\n"
        );
    }

    #[test]
    fn read_data_mode_defaults_to_none_and_reads_manual() {
        let dir = tempfile::tempdir().unwrap();
        // No manifest at all → not manual (today's behavior).
        assert_eq!(read_data_mode(dir.path()).unwrap(), DataMode::None);

        std::fs::write(
            dir.path().join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\"\n}\n",
        )
        .unwrap();
        assert_eq!(read_data_mode(dir.path()).unwrap(), DataMode::None);

        std::fs::write(
            dir.path().join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"manual\"\n}\n",
        )
        .unwrap();
        assert_eq!(read_data_mode(dir.path()).unwrap(), DataMode::Manual);
    }

    #[test]
    fn read_data_mode_rejects_an_unknown_value_loudly() {
        // A typo must fail loudly rather than silently reverting the dataset to
        // metadata-only (which would look like rdc ignoring the user).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"Manual \"\n}\n",
        )
        .unwrap();
        let err = format!("{:#}", read_data_mode(dir.path()).unwrap_err());
        assert!(err.contains("Manual "), "must echo the bad value: {err}");
        assert!(err.contains("manual"), "must name the accepted value: {err}");
        assert!(err.contains(COLLECTION_MANIFEST), "must name the file: {err}");
    }

    #[test]
    fn read_collection_name_still_tolerates_absent_and_malformed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_collection_name(dir.path()), None);
        std::fs::write(dir.path().join(COLLECTION_MANIFEST), b"not json").unwrap();
        assert_eq!(read_collection_name(dir.path()), None);
    }
```

Delete the now-superseded `read_collection_name_tolerates_absent_and_malformed` test (replaced by the last one above).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::pull::mdh`
Expected: compile error — `cannot find function manifest_bytes_merged` / `read_data_mode` / `DataMode`.

- [ ] **Step 3: Implement**

In `src/cli/pull/mdh.rs`, replace the `collection_manifest_bytes` function (lines 22-29) with:

```rust
/// The only accepted value of the manifest's optional `data` key. Its presence
/// is what opts a dataset into row-data versioning.
pub(crate) const MANUAL_DATA_VALUE: &str = "manual";

/// Whether a dataset's ROW DATA is versioned in the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataMode {
    /// Metadata only — the historical (and default) behavior: rdc never reads
    /// or writes this collection's rows.
    None,
    /// Rows are pulled to `data.jsonl`, versioned, and pushed authoritatively.
    Manual,
}

/// Read a dataset's row-data mode from its manifest. Absent manifest or absent
/// `data` key ⇒ [`DataMode::None`] (backward compatible). An unrecognised value
/// is a hard error: silently ignoring it would look exactly like rdc dropping
/// the user's opt-in.
pub(crate) fn read_data_mode(dataset_dir: &std::path::Path) -> Result<DataMode> {
    let path = dataset_dir.join(COLLECTION_MANIFEST);
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(DataMode::None);
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(DataMode::None);
    };
    match value.get("data") {
        None | Some(Value::Null) => Ok(DataMode::None),
        Some(Value::String(s)) if s == MANUAL_DATA_VALUE => Ok(DataMode::Manual),
        Some(other) => anyhow::bail!(
            "{}: unrecognised \"data\" value {other} in {COLLECTION_MANIFEST}. \
             The only accepted value is \"{MANUAL_DATA_VALUE}\" (row data versioned \
             in {}); remove the key for metadata-only.",
            dataset_dir.display(),
            crate::snapshot::mdh_data::DATA_FILE,
        ),
    }
}

/// Canonical on-disk bytes for a collection manifest, MERGING into whatever the
/// file already holds: `name` is refreshed from server truth while every other
/// key (notably the `data` opt-in) and the file's own key order are preserved.
///
/// Clobbering instead of merging is what would erase a hand-added flag on the
/// next pull. Unparseable existing bytes fall back to a fresh manifest.
/// Serialized as `to_vec_pretty` + newline, matching migrate's JSON writer so
/// a migrated manifest and a pulled one are byte-comparable.
pub(crate) fn manifest_bytes_merged(existing: Option<&[u8]>, name: &str) -> Result<Vec<u8>> {
    let mut obj = existing
        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
        .and_then(|v| match v {
            Value::Object(o) => Some(o),
            _ => None,
        })
        .unwrap_or_default();
    obj.insert("name".to_string(), Value::String(name.to_string()));
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(obj))
        .context("serializing collection manifest")?;
    bytes.push(b'\n');
    Ok(bytes)
}
```

Then update the write in sub-phase A (`src/cli/pull/mdh.rs:500-509`) to read-then-merge:

```rust
        let manifest_path = dataset_dir.join(COLLECTION_MANIFEST);
        let existing = std::fs::read(&manifest_path).ok();
        let manifest_bytes = manifest_bytes_merged(existing.as_deref(), &c.name)?;
        let needs_write = existing.as_deref() != Some(manifest_bytes.as_slice());
        if needs_write {
            std::fs::write(&manifest_path, &manifest_bytes)
                .with_context(|| format!("writing {}", manifest_path.display()))?;
            changed.insert(slug.clone());
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cli::pull::mdh`
Expected: all pass (including the pre-existing `process_counts_only_changed_datasets`, which proves no churn was introduced).

- [ ] **Step 5: Verify no other call site broke**

Run: `grep -rn "collection_manifest_bytes" src/ tests/`
Expected: no output. If anything remains, update it to `manifest_bytes_merged`.

- [ ] **Step 6: Commit**

```bash
git add src/cli/pull/mdh.rs
git commit -m "fix(mdh): merge collection.json on pull instead of clobbering it

The pull rewrote the manifest from server truth whenever bytes differed,
which would erase any user-added key. Adds the \"data\": \"manual\" opt-in
and reads it with a loud error on an unrecognised value."
```

---

### Task 4: Data Storage row verbs

**Files:**
- Modify: `src/api/data_storage.rs:1-24` (module doc), and add five methods after `aggregate` (:106)
- Test: inline `mod tests` in `src/api/data_storage.rs` (wiremock, following `create_collection_posts_expected_endpoint_and_payload` at :343)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces, all on `DataStorageClient`:
  - `pub async fn find_all(&self, collection: &str, progress: ProgressHandle) -> Result<Vec<Value>>`
  - `pub async fn count_documents(&self, collection: &str, progress: ProgressHandle) -> Result<usize>`
  - `pub async fn insert_many(&self, collection: &str, documents: &[Value], progress: ProgressHandle) -> Result<()>`
  - `pub async fn delete_many_by_ids(&self, collection: &str, ids: &[Value], progress: ProgressHandle) -> Result<usize>`
  - `pub async fn replace_one(&self, collection: &str, id: &Value, replacement: &Value, progress: ProgressHandle) -> Result<usize>`

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` in `src/api/data_storage.rs`:

```rust
    /// `find_all` must POST `data/find` with an empty query and hand back every
    /// document — one call returns the whole collection (live-verified: 1206
    /// documents in a single response).
    #[tokio::test]
    async fn find_all_posts_empty_query_and_returns_every_document() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .and(body_json(json!({ "collectionName": "GL_CODES", "query": {} })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok",
                "message": "",
                "result": [
                    { "_id": { "$oid": "6a8403a6070b60eaa348d173" }, "code": "1000" },
                    { "_id": "gl-2000", "code": "2000" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        let rows = client.find_all("GL_CODES", None).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["_id"], json!("gl-2000"));
    }

    /// `$count` returns `[{"n": N}]`, and `[]` for an empty collection — the
    /// empty case must read as 0, not as an error.
    #[tokio::test]
    async fn count_documents_handles_the_empty_collection_result() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "code": "ok", "message": "", "result": [] })),
            )
            .mount(&server)
            .await;
        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        assert_eq!(client.count_documents("GL_CODES", None).await.unwrap(), 0);

        let server2 = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "code": "ok", "message": "", "result": [{ "n": 129 }] }),
            ))
            .mount(&server2)
            .await;
        let client2 = DataStorageClient::new(server2.uri(), "TOKEN".into()).unwrap();
        assert_eq!(client2.count_documents("GL_CODES", None).await.unwrap(), 129);
    }

    /// The insert body's exact shape is load-bearing: `ordered:false` +
    /// `waitForFullWrite:true` is the combination verified against the live API.
    #[tokio::test]
    async fn insert_many_posts_the_verified_body_shape() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/insert_many"))
            .and(body_json(json!({
                "collectionName": "GL_CODES",
                "documents": [{ "code": "1000" }],
                "ordered": false,
                "waitForFullWrite": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "", "result": { "inserted_ids": ["x"] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        client
            .insert_many("GL_CODES", &[json!({ "code": "1000" })], None)
            .await
            .unwrap();
    }

    /// Deletes target raw `_id` values via `$in`, and the id list is
    /// deliberately mixed-type (ObjectId wrappers and plain scalars coexist in
    /// one collection).
    #[tokio::test]
    async fn delete_many_by_ids_posts_mixed_type_in_filter_and_returns_count() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/delete_many"))
            .and(body_json(json!({
                "collectionName": "GL_CODES",
                "filter": { "_id": { "$in": [{ "$oid": "6a8403a6070b60eaa348d173" }, "gl-2000"] } },
                "waitForFullWrite": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "", "result": { "deleted_count": 2 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        let n = client
            .delete_many_by_ids(
                "GL_CODES",
                &[json!({ "$oid": "6a8403a6070b60eaa348d173" }), json!("gl-2000")],
                None,
            )
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    /// The replacement must NOT carry `_id`: the live API rejects a replacement
    /// that alters the immutable field, and omitting it makes that unreachable.
    /// `matched_count` comes back so the caller can detect a vanished row.
    #[tokio::test]
    async fn replace_one_omits_id_from_the_replacement_and_returns_matched_count() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/replace_one"))
            .and(body_json(json!({
                "collectionName": "GL_CODES",
                "filter": { "_id": "gl-1000" },
                "replacement": { "code": "1000", "label": "new" },
                "waitForFullWrite": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "",
                "result": { "matched_count": 1, "modified_count": 1, "upserted_id": null }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "TOKEN".into()).unwrap();
        let matched = client
            .replace_one(
                "GL_CODES",
                &json!("gl-1000"),
                &json!({ "code": "1000", "label": "new" }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(matched, 1);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib api::data_storage`
Expected: compile errors — `no method named find_all` etc.

- [ ] **Step 3: Implement the five methods**

In `src/api/data_storage.rs`, after the `aggregate` method (ends :106), add:

```rust
    /// `POST /v1/data/find` with an empty query — every document in the
    /// collection, in one call. Verified live: a single response carried 1206
    /// documents (1002 documents ≈ 442 KB in 0.35 s), so no paging is needed at
    /// the scale rdc versions. Row ORDER is not stable across calls; callers
    /// impose their own (see `snapshot::mdh_data::to_jsonl`).
    ///
    /// A missing collection returns `{"code":"ok","result":[]}` — NOT a 404 — so
    /// an empty result never implies the collection is gone. Existence comes
    /// from [`list_collections`].
    pub async fn find_all(
        &self,
        collection: &str,
        progress: ProgressHandle,
    ) -> Result<Vec<Value>> {
        self.post_envelope(
            "/v1/data/find",
            json!({ "collectionName": collection, "query": {} }),
            progress,
        )
        .await
    }

    /// Row count via `POST /v1/data/aggregate [{"$count": "n"}]` — one cheap
    /// call for the size guardrail (`collections/list` carries no count). An
    /// empty collection yields `result: []`, which reads as 0.
    pub async fn count_documents(
        &self,
        collection: &str,
        progress: ProgressHandle,
    ) -> Result<usize> {
        let rows: Vec<Value> = self
            .post_envelope(
                "/v1/data/aggregate",
                json!({ "collectionName": collection, "pipeline": [{ "$count": "n" }] }),
                progress,
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| r.get("n"))
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as usize)
    }

    /// `POST /v1/data/insert_many`. `ordered: false` so one bad document does
    /// not mask the rest, `waitForFullWrite: true` so the same-cycle pull-back
    /// reads what we just wrote.
    ///
    /// A duplicate `_id` fails the call with HTTP 400 "batch op errors
    /// occurred" — and the NON-conflicting documents in the same batch are
    /// still inserted. Callers must therefore treat an error as PARTIALLY
    /// applied and re-read rather than assume a no-op.
    pub async fn insert_many(
        &self,
        collection: &str,
        documents: &[Value],
        progress: ProgressHandle,
    ) -> Result<()> {
        self.post_envelope_void(
            "/v1/data/insert_many",
            json!({
                "collectionName": collection,
                "documents": documents,
                "ordered": false,
                "waitForFullWrite": true,
            }),
            progress,
        )
        .await
    }

    /// `POST /v1/data/delete_many` filtered by raw `_id` values. The id list is
    /// intentionally mixed-type: one collection can hold both server-generated
    /// ObjectId wrappers and user-authored scalar ids, and `$in` accepts both in
    /// a single array. Returns `deleted_count`.
    pub async fn delete_many_by_ids(
        &self,
        collection: &str,
        ids: &[Value],
        progress: ProgressHandle,
    ) -> Result<usize> {
        let result: Value = self
            .post_envelope(
                "/v1/data/delete_many",
                json!({
                    "collectionName": collection,
                    "filter": { "_id": { "$in": ids } },
                    "waitForFullWrite": true,
                }),
                progress,
            )
            .await?;
        Ok(result
            .get("deleted_count")
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as usize)
    }

    /// `POST /v1/data/replace_one`, matching on `_id`. `replacement` MUST NOT
    /// contain `_id`: the API rejects a replacement that alters the immutable
    /// field ("the (immutable) field '_id' was found to have been altered"),
    /// and omitting it makes that error unreachable — the filter carries
    /// identity. No upsert: a no-match leaves the collection untouched and
    /// returns `matched_count: 0`, which the caller reads as "the row vanished
    /// between our read and our write".
    pub async fn replace_one(
        &self,
        collection: &str,
        id: &Value,
        replacement: &Value,
        progress: ProgressHandle,
    ) -> Result<usize> {
        let result: Value = self
            .post_envelope(
                "/v1/data/replace_one",
                json!({
                    "collectionName": collection,
                    "filter": { "_id": id },
                    "replacement": replacement,
                    "waitForFullWrite": true,
                }),
                progress,
            )
            .await?;
        Ok(result
            .get("matched_count")
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as usize)
    }
```

- [ ] **Step 4: Update the module doc**

In `src/api/data_storage.rs`, replace the second sentence of the module doc (lines 6-15, the "row-data WRITE verbs … are intentionally not implemented here" passage) with:

```rust
//! Collection CRUD (`collections/create`, `collections/drop`,
//! `collections/rename`) is partially implemented: only `create` is needed.
//! Row-data verbs ARE implemented, but only the SYNCHRONOUS ones —
//! `data/find`, `data/insert_many`, `data/delete_many`, `data/replace_one`,
//! `data/aggregate` — and only for datasets the snapshot flags
//! `"data": "manual"`. `data/bulk_write` is deliberately absent: it answers
//! 202 with an EMPTY `message`, so it carries no operation id and its
//! completion cannot be observed. rdc never touches the rows of a dataset that
//! is not flagged manual.
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib api::data_storage`
Expected: all pass (5 new + 5 pre-existing).

- [ ] **Step 6: Commit**

```bash
git add src/api/data_storage.rs
git commit -m "feat(mdh): add the synchronous Data Storage row verbs"
```

---

### Task 5: Row diff engine

**Files:**
- Create: `src/cli/push/mdh_data.rs`
- Modify: `src/cli/push/mod.rs:19` (add `pub mod mdh_data;` after `pub mod mdh;`)
- Test: inline `mod tests` in `src/cli/push/mdh_data.rs`

**Interfaces:**
- Consumes: `snapshot::mdh_data::{canonicalize_row, is_oid}` (Task 1).
- Produces:
  - `#[derive(Debug, Default, PartialEq)] pub(crate) struct RowDiff { pub insert: Vec<Value>, pub replace: Vec<(Value, Value)>, pub delete: Vec<Value> }`
  - `pub(crate) fn diff_rows(local: &[Value], remote: &[Value]) -> RowDiff`

`replace` pairs are `(id_value, replacement_without_id)`. `delete` holds RAW remote `_id` values. `remote` is the raw output of `find_all` (ids intact); `local` is the output of `from_jsonl`.

- [ ] **Step 1: Write the failing tests**

Create `src/cli/push/mdh_data.rs` with ONLY the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn oid(hex: &str) -> Value {
        json!({ "$oid": hex })
    }

    #[test]
    fn identical_sides_produce_no_operations() {
        let local = vec![json!({ "code": "1000" }), json!({ "_id": "gl-2", "code": "2000" })];
        let remote = vec![
            json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000" }),
            json!({ "_id": "gl-2", "code": "2000" }),
        ];
        assert_eq!(diff_rows(&local, &remote), RowDiff::default());
    }

    /// Key order and nesting must not register as a change — both sides are
    /// reduced to the same canonical form first.
    #[test]
    fn key_order_difference_is_not_a_change() {
        let local = vec![json!({ "a": 1, "b": { "d": 4, "c": 3 } })];
        let remote = vec![json!({ "_id": oid("aa"), "b": { "c": 3, "d": 4 }, "a": 1 })];
        assert_eq!(diff_rows(&local, &remote), RowDiff::default());
    }

    #[test]
    fn keyed_row_edit_becomes_an_in_place_replace_without_id() {
        let local = vec![json!({ "_id": "gl-1000", "code": "1000", "label": "new" })];
        let remote = vec![json!({ "_id": "gl-1000", "code": "1000", "label": "old" })];
        let d = diff_rows(&local, &remote);
        assert!(d.insert.is_empty() && d.delete.is_empty());
        assert_eq!(
            d.replace,
            vec![(json!("gl-1000"), json!({ "code": "1000", "label": "new" }))],
            "identity is carried by the filter; the replacement must omit _id"
        );
    }

    #[test]
    fn keyed_row_missing_remotely_is_inserted_with_its_id() {
        let local = vec![json!({ "_id": "gl-1000", "code": "1000" })];
        let d = diff_rows(&local, &[]);
        assert_eq!(d.insert, vec![json!({ "_id": "gl-1000", "code": "1000" })]);
        assert!(d.replace.is_empty() && d.delete.is_empty());
    }

    #[test]
    fn keyed_row_missing_locally_is_deleted_by_its_raw_id() {
        let remote = vec![json!({ "_id": "gl-1000", "code": "1000" })];
        let d = diff_rows(&[], &remote);
        assert_eq!(d.delete, vec![json!("gl-1000")]);
        assert!(d.insert.is_empty() && d.replace.is_empty());
    }

    #[test]
    fn unkeyed_row_edit_becomes_a_delete_plus_an_insert() {
        // Without a business key there is nothing to pair old and new by, so an
        // edit is expressed as removing the old row and adding the new one.
        let local = vec![json!({ "code": "1000", "label": "new" })];
        let remote = vec![json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000", "label": "old" })];
        let d = diff_rows(&local, &remote);
        assert_eq!(d.insert, vec![json!({ "code": "1000", "label": "new" })]);
        assert_eq!(d.delete, vec![oid("6a8403a6070b60eaa348d173")]);
        assert!(d.replace.is_empty());
    }

    /// Unkeyed rows are a MULTISET: two identical rows are two rows. The diff
    /// moves only the surplus, in either direction.
    #[test]
    fn unkeyed_duplicates_are_diffed_by_count() {
        let row = json!({ "code": "1000" });
        // local wants 3, remote has 1 → insert 2.
        let d = diff_rows(
            &[row.clone(), row.clone(), row.clone()],
            &[json!({ "_id": oid("a1"), "code": "1000" })],
        );
        assert_eq!(d.insert, vec![row.clone(), row.clone()]);
        assert!(d.delete.is_empty());

        // local wants 1, remote has 3 → delete 2 (the surplus, deterministically).
        let d = diff_rows(
            &[row.clone()],
            &[
                json!({ "_id": oid("a1"), "code": "1000" }),
                json!({ "_id": oid("a2"), "code": "1000" }),
                json!({ "_id": oid("a3"), "code": "1000" }),
            ],
        );
        assert!(d.insert.is_empty());
        assert_eq!(d.delete, vec![oid("a2"), oid("a3")]);
    }

    /// A hand-written `$oid` `_id` on the local side is NOT a business key: it
    /// is dropped by canonicalization, so the row diffs as unkeyed content and
    /// does not churn against the identical remote row.
    #[test]
    fn hand_written_server_id_locally_is_not_treated_as_a_key() {
        let local = vec![json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000" })];
        let remote = vec![json!({ "_id": oid("6a8403a6070b60eaa348d173"), "code": "1000" })];
        assert_eq!(diff_rows(&local, &remote), RowDiff::default());
    }

    #[test]
    fn output_is_deterministic_regardless_of_input_order() {
        let a = json!({ "code": "1000" });
        let b = json!({ "code": "2000" });
        let one = diff_rows(&[a.clone(), b.clone()], &[]);
        let two = diff_rows(&[b.clone(), a.clone()], &[]);
        assert_eq!(one, two, "op order must not depend on input order");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::push::mdh_data`
Expected: compile error — the module is not declared and `diff_rows` does not exist.

- [ ] **Step 3: Declare the module**

In `src/cli/push/mod.rs`, after line 19 (`pub mod mdh;`):

```rust
pub mod mdh_data;
```

- [ ] **Step 4: Implement**

Prepend to `src/cli/push/mdh_data.rs`:

```rust
//! Push driver for MDH row data (`data.jsonl`) — the hybrid keyed diff.
//!
//! The local file is authoritative: after a push the env holds exactly the rows
//! the file lists. How that is achieved matters, though, so this is NOT a
//! delete-everything-then-reinsert:
//!
//! - a row carrying an explicit (non-ObjectId) `_id` is identified BY that id,
//!   so editing it is one in-place `replace_one` and its identity survives;
//! - a row without one is identified by its canonical content, as a multiset,
//!   so only genuinely new rows are inserted and only genuinely surplus rows
//!   are deleted. Untouched rows are never rewritten — no empty window, no
//!   ObjectId churn.
//!
//! Deletes are applied BEFORE inserts so a unique index survives a row edit:
//! the outgoing row releases its key before the incoming row claims it.

use crate::snapshot::mdh_data::canonicalize_row;
use serde_json::Value;
use std::collections::BTreeMap;

/// The operations that make a collection match `data.jsonl`.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RowDiff {
    /// Documents to insert, canonical form (may carry an explicit `_id`).
    pub insert: Vec<Value>,
    /// `(id, replacement)` pairs for in-place replace. The replacement never
    /// contains `_id` — the filter carries identity and the field is immutable.
    pub replace: Vec<(Value, Value)>,
    /// RAW remote `_id` values to delete.
    pub delete: Vec<Value>,
}

/// The explicit, user-authored `_id` of a canonical row, if any. A
/// server-generated ObjectId is not one — `canonicalize_row` has already
/// dropped it, so this returns `None` for those.
fn explicit_id(canonical: &Value) -> Option<Value> {
    canonical.get("_id").cloned()
}

/// Canonical JSON text of a value — the map key for both identity flavors.
fn key_of(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// A canonical row with its `_id` removed: the shape `replace_one` wants.
fn without_id(canonical: &Value) -> Value {
    let mut out = canonical.clone();
    if let Value::Object(obj) = &mut out {
        obj.shift_remove("_id");
    }
    out
}

/// Diff local rows (from `data.jsonl`) against raw remote rows (from
/// `find_all`, `_id`s intact). Output order is derived from sorted maps, so it
/// is deterministic and independent of input order.
pub(crate) fn diff_rows(local: &[Value], remote: &[Value]) -> RowDiff {
    // key → canonical row
    let mut local_keyed: BTreeMap<String, Value> = BTreeMap::new();
    // canonical json → (row, wanted count)
    let mut local_unkeyed: BTreeMap<String, (Value, usize)> = BTreeMap::new();
    for row in local {
        let canonical = canonicalize_row(row);
        match explicit_id(&canonical) {
            Some(id) => {
                local_keyed.insert(key_of(&id), canonical);
            }
            None => {
                let k = key_of(&canonical);
                local_unkeyed.entry(k).or_insert((canonical, 0)).1 += 1;
            }
        }
    }

    // key → (raw id, canonical row)
    let mut remote_keyed: BTreeMap<String, (Value, Value)> = BTreeMap::new();
    // canonical json → raw ids, sorted so the surplus we drop is deterministic
    let mut remote_unkeyed: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in remote {
        let raw_id = row.get("_id").cloned().unwrap_or(Value::Null);
        let canonical = canonicalize_row(row);
        match explicit_id(&canonical) {
            Some(id) => {
                remote_keyed.insert(key_of(&id), (raw_id, canonical));
            }
            None => remote_unkeyed
                .entry(key_of(&canonical))
                .or_default()
                .push(raw_id),
        }
    }
    for ids in remote_unkeyed.values_mut() {
        ids.sort_by_key(key_of);
    }

    let mut diff = RowDiff::default();

    for (key, local_row) in &local_keyed {
        match remote_keyed.get(key) {
            None => diff.insert.push(local_row.clone()),
            Some((_, remote_row)) if remote_row != local_row => {
                if let Some(id) = explicit_id(local_row) {
                    diff.replace.push((id, without_id(local_row)));
                }
            }
            Some(_) => {}
        }
    }
    for (key, (raw_id, _)) in &remote_keyed {
        if !local_keyed.contains_key(key) {
            diff.delete.push(raw_id.clone());
        }
    }

    for (key, (row, wanted)) in &local_unkeyed {
        let have = remote_unkeyed.get(key).map_or(0, |ids| ids.len());
        for _ in have..*wanted {
            diff.insert.push(row.clone());
        }
    }
    for (key, ids) in &remote_unkeyed {
        let wanted = local_unkeyed.get(key).map_or(0, |(_, n)| *n);
        for raw_id in ids.iter().skip(wanted) {
            diff.delete.push(raw_id.clone());
        }
    }

    diff
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib cli::push::mdh_data`
Expected: 9 passed.

- [ ] **Step 6: Commit**

```bash
git add src/cli/push/mdh_data.rs src/cli/push/mod.rs
git commit -m "feat(mdh): hybrid keyed diff for MDH row data"
```

---

### Task 6: Row push driver

**Files:**
- Modify: `src/cli/push/mdh_data.rs` (add `push_dataset_data` + chunk constant above the tests)
- Test: inline `mod tests` in `src/cli/push/mdh_data.rs`

**Interfaces:**
- Consumes: `diff_rows` / `RowDiff` (Task 5); `DataStorageClient::{find_all, insert_many, delete_many_by_ids, replace_one}` (Task 4); `snapshot::mdh_data::{from_jsonl, ROW_HARD_LIMIT, ROW_WARN_THRESHOLD}` (Task 1); `push::mdh::{classify_delete_gate, DeleteGate}` (`src/cli/push/mdh.rs:640-664`); `state::{base_cache, content_hash, Lockfile, ObjectEntry}`.
- Produces:

```rust
#[allow(clippy::too_many_arguments)]
pub async fn push_dataset_data(
    client: &crate::api::DataStorageClient,
    lockfile: &mut crate::state::Lockfile,
    collection_name: &str,
    slug: &str,
    paths: &crate::paths::Paths,
    allow_deletes: bool,
    interactive: bool,
    progress: &std::sync::Arc<crate::log::Log>,
) -> anyhow::Result<usize>
```

Returns the number of API write operations performed (0 when there is nothing to do, including when the file is absent).

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/cli/push/mdh_data.rs`:

```rust
    use crate::api::DataStorageClient;
    use crate::state::{Lockfile, content_hash};
    use std::sync::Arc;

    fn log() -> Arc<crate::log::Log> {
        crate::log::Log::new(crate::cli::resolve::ColorMode::Plain)
    }

    /// Seed a manual dataset on disk: manifest with the flag + the given rows.
    fn seed(paths: &crate::paths::Paths, slug: &str, jsonl: &[u8]) {
        let dir = paths.dataset_dir(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(crate::cli::pull::mdh::COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"manual\"\n}\n",
        )
        .unwrap();
        std::fs::write(paths.dataset_data(slug), jsonl).unwrap();
    }

    /// An ABSENT data.jsonl means "not pulled yet", never "the env should have
    /// zero rows". Push must make no calls at all — the safety rule that keeps
    /// a hand-added flag from wiping a collection.
    #[tokio::test]
    async fn absent_data_file_is_never_authoritative() {
        let server = wiremock::MockServer::start().await;
        // No mocks mounted: any request would 404 and fail the call.
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.dataset_dir("gl-codes")).unwrap();
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 0, "an absent file must produce no writes");
        assert!(lf.objects.get("mdh_data").is_none(), "nothing to record");
    }

    /// Local rows the env lacks are inserted, and a fully-applied push advances
    /// both the lockfile hash and the base cache — the invariant that makes the
    /// next sync see the dataset as clean.
    #[tokio::test]
    async fn inserts_missing_rows_then_records_lockfile_and_base() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/insert_many"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": { "inserted_ids": ["a"] } }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let jsonl = b"{\"code\":\"1000\"}\n".to_vec();
        seed(&paths, "gl-codes", &jsonl);
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 1);
        assert_eq!(
            lf.objects["mdh_data"]["gl-codes"].content_hash.as_deref(),
            Some(content_hash(&jsonl, &Lockfile::default()).as_str())
        );
        assert_eq!(
            crate::state::base_cache::read(&paths, &paths.dataset_data("gl-codes")).unwrap(),
            Some(jsonl),
            "base cache must move in lockstep with the lockfile"
        );
    }

    /// Deleting rows is destructive, so it rides the same gate index drops do:
    /// non-interactive without --allow-deletes must BAIL, having written nothing.
    #[tokio::test]
    async fn pending_deletes_bail_without_allow_deletes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "_id": { "$oid": "a1" }, "code": "9999" }]
            })))
            .mount(&server)
            .await;
        // No delete mock: if a delete were attempted the test would fail.

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        seed(&paths, "gl-codes", b"");
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let err = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--allow-deletes"), "must name the flag: {msg}");
        assert!(msg.contains("gl-codes"), "must name the dataset: {msg}");
        assert!(lf.objects.get("mdh_data").is_none(), "must not record a bailed push");
    }

    #[tokio::test]
    async fn allow_deletes_applies_deletes_before_inserts() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "_id": { "$oid": "a1" }, "code": "old" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/delete_many"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": { "deleted_count": 1 } }),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/insert_many"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": { "inserted_ids": ["b"] } }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        seed(&paths, "gl-codes", b"{\"code\":\"new\"}\n");
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, true, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 2, "one delete + one insert");
    }

    /// A keyed row edit is one in-place replace — no delete, no insert.
    #[tokio::test]
    async fn keyed_edit_issues_a_single_replace() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "_id": "gl-1000", "label": "old" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/replace_one"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": { "matched_count": 1 } }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        seed(&paths, "gl-codes", b"{\"_id\":\"gl-1000\",\"label\":\"new\"}\n");
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 1);
    }

    /// Nothing to do → no writes, but the lockfile/base still get recorded so a
    /// first sync of an already-matching dataset converges instead of retrying.
    #[tokio::test]
    async fn no_op_push_records_state_without_writing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "_id": { "$oid": "a1" }, "code": "1000" }]
            })))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        seed(&paths, "gl-codes", b"{\"code\":\"1000\"}\n");
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let ops = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap();
        assert_eq!(ops, 0);
        assert!(lf.objects["mdh_data"].contains_key("gl-codes"));
    }

    /// The hard ceiling protects git from an import-fed table: refuse before any
    /// write, naming the count and the way out.
    #[tokio::test]
    async fn local_rows_over_the_hard_limit_are_refused() {
        let server = wiremock::MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let mut jsonl = Vec::new();
        for i in 0..(crate::snapshot::mdh_data::ROW_HARD_LIMIT + 1) {
            jsonl.extend_from_slice(format!("{{\"code\":\"{i:06}\"}}\n").as_bytes());
        }
        seed(&paths, "gl-codes", &jsonl);
        let client = DataStorageClient::new(server.uri(), "t".into()).unwrap();
        let mut lf = Lockfile::default();

        let err = push_dataset_data(
            &client, &mut lf, "GL_CODES", "gl-codes", &paths, false, false, &log(),
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("10001"), "must state the count: {msg}");
        assert!(msg.contains("data"), "must point at the opt-out: {msg}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::push::mdh_data`
Expected: compile error — `cannot find function push_dataset_data`.

- [ ] **Step 3: Implement**

Add to `src/cli/push/mdh_data.rs`, above the test module (extend the existing `use` block at the top of the file):

```rust
use crate::api::DataStorageClient;
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::snapshot::mdh_data::{ROW_HARD_LIMIT, ROW_WARN_THRESHOLD, from_jsonl};
use crate::state::{Lockfile, ObjectEntry, content_hash};
use anyhow::{Context, Result};
use std::sync::Arc;

/// Documents per `insert_many` / ids per `delete_many`. 1 000 documents ×
/// 20 fields (≈440 KB) was measured at ~1.1 s against the live API, so 500
/// leaves comfortable headroom.
const WRITE_CHUNK: usize = 500;

/// Make `collection_name` hold exactly the rows in `data.jsonl`.
///
/// An ABSENT `data.jsonl` is never authoritative: it means the dataset was
/// flagged manual but has not been pulled yet, NOT that the env should be
/// emptied. Such a dataset is skipped entirely (0 ops, no reads, no writes).
/// Only a present file — including a deliberately 0-byte one — expresses
/// "these are all the rows".
///
/// The lockfile hash and the base cache are advanced only when the push FULLY
/// reconciled the env (no gated skip, no error). A partial push leaves the
/// dataset looking locally-diverged, so the next sync re-reads and re-diffs —
/// which is the correct recovery given that a failed `insert_many` is
/// partially applied.
#[allow(clippy::too_many_arguments)]
pub async fn push_dataset_data(
    client: &DataStorageClient,
    lockfile: &mut Lockfile,
    collection_name: &str,
    slug: &str,
    paths: &Paths,
    allow_deletes: bool,
    interactive: bool,
    progress: &Arc<Log>,
) -> Result<usize> {
    let data_path = paths.dataset_data(slug);
    let Ok(local_raw) = std::fs::read(&data_path) else {
        return Ok(0); // not pulled yet — never authoritative
    };

    let local_rows = from_jsonl(&local_raw, &data_path.display().to_string())?;
    if local_rows.len() > ROW_HARD_LIMIT {
        anyhow::bail!(
            "mdh/{slug}: {} rows in {} exceeds rdc's {ROW_HARD_LIMIT}-row ceiling for \
             versioned MDH data. Remove the \"data\" key from {} to stop versioning this \
             dataset's rows (its indexes and name stay managed).",
            local_rows.len(),
            data_path.display(),
            crate::cli::pull::mdh::COLLECTION_MANIFEST,
        );
    }
    if local_rows.len() > ROW_WARN_THRESHOLD {
        progress.event(
            Action::Warn,
            &format!(
                "mdh/{slug}: {} rows is large for a git-versioned dataset (warns above \
                 {ROW_WARN_THRESHOLD})",
                local_rows.len()
            ),
        );
    }

    let remote_rows = client
        .find_all(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("reading rows of '{collection_name}'"))?;

    let diff = diff_rows(&local_rows, &remote_rows);

    // Deletions ride the same gate as index drops.
    let pending = diff.delete.len();
    let mut deletes: &[Value] = &diff.delete;
    let mut skipped = false;
    match crate::cli::push::mdh::classify_delete_gate(pending, allow_deletes, interactive) {
        crate::cli::push::mdh::DeleteGate::Proceed => {}
        crate::cli::push::mdh::DeleteGate::Bail => {
            anyhow::bail!(
                "mdh/{slug}: {pending} row(s) present on the env are absent from {} and \
                 would be DELETED, but --allow-deletes was not passed. Re-run with \
                 --allow-deletes to authorise it, or restore the rows in that file to cancel.",
                data_path.display()
            );
        }
        crate::cli::push::mdh::DeleteGate::Prompt => {
            if prompt_confirm_row_deletes(progress, collection_name, pending)? {
                // proceed
            } else {
                deletes = &[];
                skipped = true;
                progress.event(
                    Action::Skip,
                    &format!("mdh/{slug} {pending} row deletion(s) skipped"),
                );
            }
        }
    }

    let mut ops = 0usize;

    // Deletes FIRST: a row edit on a uniquely-indexed field needs the outgoing
    // row gone before the incoming row claims the key.
    for chunk in deletes.chunks(WRITE_CHUNK) {
        client
            .delete_many_by_ids(collection_name, chunk, Some(progress.clone()))
            .await
            .with_context(|| format!("deleting rows from '{collection_name}'"))?;
        progress.event(
            Action::Delete,
            &format!("mdh/{slug} {} row(s)", chunk.len()),
        );
        ops += 1;
    }

    for (id, replacement) in &diff.replace {
        let matched = client
            .replace_one(collection_name, id, replacement, Some(progress.clone()))
            .await
            .with_context(|| format!("replacing row {id} in '{collection_name}'"))?;
        if matched == 0 {
            progress.event(
                Action::Warn,
                &format!(
                    "mdh/{slug} row {id} vanished between read and write; will retry next sync"
                ),
            );
            skipped = true;
        } else {
            progress.event(Action::Patch, &format!("mdh/{slug} row {id}"));
            ops += 1;
        }
    }

    for chunk in diff.insert.chunks(WRITE_CHUNK) {
        client
            .insert_many(collection_name, chunk, Some(progress.clone()))
            .await
            .with_context(|| format!("inserting rows into '{collection_name}'"))?;
        progress.event(Action::Post, &format!("mdh/{slug} {} row(s)", chunk.len()));
        ops += 1;
    }

    if !skipped {
        lockfile.upsert(
            "mdh_data",
            slug,
            ObjectEntry {
                id: 0,
                modified_at: None,
                content_hash: Some(content_hash(&local_raw, &Lockfile::default())),
                secrets_hash: None,
            },
        );
        crate::state::base_cache::write(paths, &data_path, &local_raw)
            .with_context(|| format!("writing base cache for mdh/{slug} rows"))?;
    }

    Ok(ops)
}

/// Interactive [y/N] confirmation for deleting remote rows absent from
/// `data.jsonl`. Byte-for-byte the shape of `prompt_confirm_index_drops`
/// (`push/mdh.rs:668`): `with_prompt` clears any active status line, and
/// `read_line_coordinated()` (no arguments, `io::Result<Option<String>>`) is the
/// single chokepoint that also rings the watch attention bell.
fn prompt_confirm_row_deletes(
    progress: &Arc<Log>,
    collection_name: &str,
    pending: usize,
) -> Result<bool> {
    progress.with_prompt(|| -> Result<bool> {
        use std::io::Write;
        eprintln!();
        eprintln!(
            "{pending} row(s) on '{collection_name}' are absent from data.jsonl and would \
             be DELETED."
        );
        eprint!("Proceed with the deletion(s)? [y/N] ");
        std::io::stderr().flush().ok();
        let ans = crate::cli::stdin_coord::read_line_coordinated()?
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        Ok(ans == "y" || ans == "yes")
    })
}
```

Also add the duplicate-row warning the spec calls for, right after `local_rows` is
parsed (identical rows are legal as a multiset, but in a lookup table they are
almost always a mistake):

```rust
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let duplicates = local_rows.iter().filter(|r| !seen.insert(key_of(r))).count();
    if duplicates > 0 {
        progress.event(
            Action::Warn,
            &format!(
                "mdh/{slug}: {duplicates} duplicate row(s) in {} — kept as-is (rows are a \
                 multiset), but duplicates in a lookup table are usually unintended",
                data_path.display()
            ),
        );
    }
```

- [ ] **Step 4: Check it builds**

Run: `cargo build`
Expected: clean build. `classify_delete_gate` and `DeleteGate` are already `pub(crate)` in `push/mdh.rs:640-664`, so no visibility change is needed.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib cli::push::mdh_data`
Expected: all pass (9 diff tests + 7 driver tests).

- [ ] **Step 6: Commit**

```bash
git add src/cli/push/mdh_data.rs
git commit -m "feat(mdh): push MDH row data authoritatively from data.jsonl"
```

---

### Task 7: Row pull

**Files:**
- Modify: `src/cli/pull/mdh.rs` (add `pull_dataset_data`, call it from sub-phase C of `process`)
- Test: inline `mod tests` in `src/cli/pull/mdh.rs`

**Interfaces:**
- Consumes: `DataStorageClient::{find_all, count_documents}` (Task 4); `snapshot::mdh_data::{to_jsonl, ROW_HARD_LIMIT, ROW_WARN_THRESHOLD}` (Task 1); `read_data_mode` / `DataMode` (Task 3); `decide_pull_action` / `apply_pull_action` / `record_object` (`pull/common.rs:568,685,466`).
- Produces:

```rust
pub(crate) async fn pull_dataset_data(
    ctx: &mut PullCtx<'_>,
    client: &DataStorageClient,
    collection_name: &str,
    slug: &str,
    progress: &Arc<Log>,
) -> Result<(bool, usize)>   // (changed, conflicts)
```

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/cli/pull/mdh.rs`:

```rust
    /// A dataset with no `"data": "manual"` flag must cost ZERO row calls — the
    /// backward-compatibility guarantee for every existing project.
    #[tokio::test]
    async fn process_makes_no_row_calls_for_a_non_manual_dataset() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;
        // /v1/data/find and /v1/data/aggregate are deliberately NOT mounted:
        // any row call would 404 and fail the pull.

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.mdh_dir()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let rossum = crate::api::RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "t".to_string(),
        )
        .unwrap();
        let mut lockfile = crate::state::Lockfile::default();
        let subset: BTreeSet<(String, String)> =
            [("mdh".to_string(), "gl-codes".to_string())].into_iter().collect();
        let listed = MdhListed {
            client: DataStorageClient::new(server.uri(), "t".to_string()).unwrap(),
            collections: vec![Collection {
                name: "gl-codes".to_string(),
                extra: Default::default(),
            }],
            available: true,
        };
        let mut ctx = PullCtx {
            paths: &paths,
            client: &rossum,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };
        process(&mut ctx, listed, &subset, &progress).await.unwrap();
        assert!(
            !paths.dataset_data("gl-codes").exists(),
            "a non-manual dataset must get no data.jsonl"
        );
        assert!(lockfile.objects.get("mdh_data").is_none());
    }

    /// A manual dataset's rows land in canonical form, and the lockfile +
    /// base cache record them so the next cycle is a no-op.
    #[tokio::test]
    async fn pull_dataset_data_writes_canonical_rows_and_records_state() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": [{ "n": 2 }] }),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [
                    { "_id": { "$oid": "a2" }, "label": "Travel", "code": "2000" },
                    { "_id": { "$oid": "a1" }, "label": "Office supplies", "code": "1000" }
                ]
            })))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let dir = paths.dataset_dir("gl-codes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"manual\"\n}\n",
        )
        .unwrap();

        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let rossum = crate::api::RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "t".to_string(),
        )
        .unwrap();
        let client = DataStorageClient::new(server.uri(), "t".to_string()).unwrap();
        let mut lockfile = crate::state::Lockfile::default();
        let mut ctx = PullCtx {
            paths: &paths,
            client: &rossum,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };

        let (changed, conflicts) =
            pull_dataset_data(&mut ctx, &client, "GL_CODES", "gl-codes", &progress)
                .await
                .unwrap();
        assert!(changed);
        assert_eq!(conflicts, 0);
        // Server ids stripped, keys sorted, lines sorted.
        assert_eq!(
            std::fs::read_to_string(paths.dataset_data("gl-codes")).unwrap(),
            "{\"code\":\"1000\",\"label\":\"Office supplies\"}\n\
             {\"code\":\"2000\",\"label\":\"Travel\"}\n"
        );
        assert!(lockfile.objects["mdh_data"].contains_key("gl-codes"));

        // Second pull over identical remote state changes nothing.
        let (changed2, _) =
            pull_dataset_data(&mut ctx, &client, "GL_CODES", "gl-codes", &progress)
                .await
                .unwrap();
        assert!(!changed2, "an unchanged re-pull must report no change");
    }

    #[tokio::test]
    async fn pull_dataset_data_refuses_a_collection_over_the_hard_limit() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "n": crate::snapshot::mdh_data::ROW_HARD_LIMIT + 1 }]
            })))
            .mount(&server)
            .await;
        // /v1/data/find not mounted: the guardrail must refuse BEFORE reading.

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let dir = paths.dataset_dir("gl-codes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"manual\"\n}\n",
        )
        .unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let rossum = crate::api::RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "t".to_string(),
        )
        .unwrap();
        let client = DataStorageClient::new(server.uri(), "t".to_string()).unwrap();
        let mut lockfile = crate::state::Lockfile::default();
        let mut ctx = PullCtx {
            paths: &paths,
            client: &rossum,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };
        let err = pull_dataset_data(&mut ctx, &client, "GL_CODES", "gl-codes", &progress)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("10001"), "must state the count: {msg}");
        assert!(!paths.dataset_data("gl-codes").exists(), "must write nothing");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::pull::mdh`
Expected: compile error — `cannot find function pull_dataset_data`.

- [ ] **Step 3: Implement `pull_dataset_data`**

Add to `src/cli/pull/mdh.rs`, after `fetch_index_set` (:391):

```rust
/// Pull one manual dataset's rows into `data.jsonl`.
///
/// Returns `(changed, conflicts)`. Costs two calls (`$count` for the guardrail,
/// then one `find`) and is invoked ONLY for datasets flagged `"data": "manual"`,
/// so a metadata-only dataset stays exactly as cheap as it is today.
pub(crate) async fn pull_dataset_data(
    ctx: &mut PullCtx<'_>,
    client: &DataStorageClient,
    collection_name: &str,
    slug: &str,
    progress: &Arc<Log>,
) -> Result<(bool, usize)> {
    use crate::snapshot::mdh_data::{ROW_HARD_LIMIT, ROW_WARN_THRESHOLD, to_jsonl};

    // Guardrail first: refuse an oversized collection BEFORE reading it, so a
    // mis-flagged import-fed dataset can never be dragged into the snapshot.
    let count = client
        .count_documents(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("counting rows of '{collection_name}'"))?;
    if count > ROW_HARD_LIMIT {
        anyhow::bail!(
            "mdh/{slug}: '{collection_name}' holds {count} rows, over rdc's \
             {ROW_HARD_LIMIT}-row ceiling for versioned MDH data. Remove the \"data\" key \
             from {}/{COLLECTION_MANIFEST} to stop versioning this dataset's rows \
             (its name and indexes stay managed).",
            ctx.paths.dataset_dir(slug).display(),
        );
    }
    if count > ROW_WARN_THRESHOLD {
        progress.event(
            Action::Warn,
            &format!(
                "mdh/{slug}: {count} rows is large for a git-versioned dataset \
                 (warns above {ROW_WARN_THRESHOLD})"
            ),
        );
    }

    let rows = client
        .find_all(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("reading rows of '{collection_name}'"))?;

    // An import-fed dataset carries the import extension's per-row digest.
    // Versioning it means rdc's authoritative pushes fight that hook for
    // ownership of the rows — warn, but do what the user asked.
    if rows
        .iter()
        .any(|r| r.get("__digest_md5").is_some())
    {
        progress.event(
            Action::Warn,
            &format!(
                "mdh/{slug}: rows carry __digest_md5, so this dataset looks maintained by \
                 an MDH import hook. rdc will treat data.jsonl as authoritative and may \
                 undo the hook's writes."
            ),
        );
    }

    let proposed = to_jsonl(&rows)?;
    let data_path = ctx.paths.dataset_data(slug);
    let base = ctx
        .lockfile
        .objects
        .get("mdh_data")
        .and_then(|m| m.get(slug))
        .and_then(|e| e.content_hash.clone());
    let (action, remote_hash) = decide_pull_action(&data_path, base.as_deref(), &proposed)?;
    let conflicts = usize::from(action == PullAction::Conflict);

    // KeepLocal: the local file diverged while the remote matched base. MDH
    // bypasses the classifier, so this pull runs right AFTER the push every
    // cycle — landing here means the push did NOT reconcile (a gated delete
    // skip, `--no-push`, a vanished row). Advancing base to the LOCAL content
    // would starve the next cycle's push gate and make the cycle after revert
    // the file. Preserve the prior base so the push retries until it lands.
    // Mirrors the identical rule on the indexes leg.
    let recorded = if action == PullAction::KeepLocal {
        base.clone().unwrap_or(remote_hash)
    } else {
        apply_pull_action(
            action,
            &data_path,
            &proposed,
            remote_hash,
            ctx.interactive,
            progress,
            ctx.paths.env(),
            base.as_deref(),
            Some(ctx.paths),
        )?
    };
    record_object(ctx.lockfile, "mdh_data", slug, 0, None, Some(recorded));

    Ok((
        matches!(action, PullAction::Write | PullAction::Conflict),
        conflicts,
    ))
}
```

- [ ] **Step 4: Call it from `process`**

In `src/cli/pull/mdh.rs` sub-phase C, inside the `for (slug, dataset_dir, _c) in &dataset_dirs` loop, after the `indexes.json` block that ends with the `changed.insert(slug.clone());` guard, add:

```rust
        // Row data — only for datasets that opted in. `read_data_mode` errors on
        // a malformed flag, which surfaces here rather than being ignored.
        if read_data_mode(dataset_dir)? == DataMode::Manual {
            let name = read_collection_name(dataset_dir)
                .unwrap_or_else(|| dataset_dir.file_name().unwrap().to_string_lossy().into_owned());
            let (data_changed, data_conflicts) =
                pull_dataset_data(ctx, &client, &name, slug, progress).await?;
            conflicts += data_conflicts;
            if data_changed {
                changed.insert(slug.clone());
            }
        }
```

Note: the manifest has just been (re)written in sub-phase A with server truth, so `read_collection_name` yields the live collection name here.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib cli::pull::mdh`
Expected: all pass, including the pre-existing `process_counts_only_changed_datasets`.

- [ ] **Step 6: Commit**

```bash
git add src/cli/pull/mdh.rs
git commit -m "feat(mdh): pull manual dataset rows into data.jsonl"
```

---

### Task 8: Sync wiring

**Files:**
- Modify: `src/cli/sync/execute.rs:3706-3746` (stage 1 — add the data push), `:3748-3850` (stage 2 — push data after a create), `:2046-2060` (`remove_mdh_dataset` — forget the data mirror and lockfile entry)
- Modify: `src/state/lockfile.rs:548` (sentinel-kind test list), `src/snapshot/refs.rs:21` (`is_portable_kind`)
- Test: inline `mod tests` in `src/cli/sync/execute.rs` (extend the `seed_mdh_dataset` helper at :5292 and the prune tests), plus `src/snapshot/refs.rs` tests

**Interfaces:**
- Consumes: `push::mdh_data::push_dataset_data` (Task 6); `pull::mdh::{read_data_mode, DataMode}` (Task 3).
- Produces: no new public API.

- [ ] **Step 1: Write the failing tests**

In `src/snapshot/refs.rs` tests, beside the existing `mdh_indexes` assertion (:220):

```rust
        assert!(!is_portable_kind("mdh_data"));
```

In `src/cli/sync/execute.rs` tests, add after `prune_mdh_orphans_use_env_removes_dataset_base_and_lockfile`:

```rust
    /// Pruning an orphaned dataset must take its ROW data with it: the file
    /// (via the dir removal), the base-cache mirror, and the `mdh_data`
    /// lockfile entry. A surviving entry would re-flag the dataset forever.
    #[tokio::test]
    async fn prune_mdh_orphans_also_removes_row_data_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let mut lockfile = Lockfile::default();
        // Seeds indexes.json byte-clean vs the lockfile hash, so the orphan
        // auto-prunes without a prompt (same path as the neighbouring test).
        let mirror = seed_mdh_dataset(&paths, &mut lockfile, "gl-codes-2");
        let data = paths.dataset_data("gl-codes-2");
        let rows = b"{\"code\":\"1000\"}\n";
        std::fs::write(&data, rows).unwrap();
        crate::state::base_cache::write(&paths, &data, rows).unwrap();
        lockfile.upsert(
            "mdh_data",
            "gl-codes-2",
            crate::state::ObjectEntry {
                id: 0,
                modified_at: None,
                content_hash: Some(crate::state::content_hash(rows, &Lockfile::default())),
                secrets_hash: None,
            },
        );

        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);
        let pruned = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                interactive: true,
            };
            prune_mdh_orphans(&mut ctx, &BTreeSet::new(), Cursor::new(b""), true, &progress)
                .await
                .expect("unchanged orphan must auto-prune")
        };

        assert_eq!(pruned, 1);
        assert!(!mirror.exists(), "indexes base mirror removed");
        assert!(!data.exists(), "row data file removed with the dataset dir");
        assert_eq!(
            crate::state::base_cache::read(&paths, &data).unwrap(),
            None,
            "row-data base mirror must be forgotten"
        );
        assert!(
            lockfile
                .objects
                .get("mdh_data")
                .and_then(|m| m.get("gl-codes-2"))
                .is_none(),
            "mdh_data lockfile entry must be dropped"
        );
    }
```

`seed_mdh_dataset` (`:5292`), `Paths`, `Lockfile`, `RossumClient`, `Log`, `Cursor`, `BTreeMap`, and `BTreeSet` are all already in scope in that test module — this test is the same shape as `prune_mdh_orphans_unchanged_auto_prunes_without_prompt` (`:5323`), with the row-data assertions added.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib snapshot::refs && cargo test --lib cli::sync::execute::tests::prune_mdh`
Expected: the refs assertion fails (`mdh_data` is currently "portable"); the prune test fails on the surviving `mdh_data` entry.

- [ ] **Step 3: Make `mdh_data` a non-portable sentinel kind**

`src/snapshot/refs.rs:21`:

```rust
    !matches!(kind, "organization" | "mdh_indexes" | "mdh_data")
```

`src/state/lockfile.rs`: extend the sentinel-kind list at :548 (the test asserting id-0 kinds yield no URL) to cover `"mdh_data"` alongside `"mdh_indexes"`, following the existing assertion style.

- [ ] **Step 4: Extend `remove_mdh_dataset`**

`src/cli/sync/execute.rs:2046-2060` — add the data-file forget before the existing lockfile drop:

```rust
    // Row data (manual datasets): the file itself went with the dir removal
    // above; its base mirror and lockfile entry must go too.
    let data_path = ctx.paths.dataset_data(slug);
    crate::state::base_cache::forget(ctx.paths, &data_path).ok();
    drop_lockfile_entry(ctx, "mdh_data", slug);
```

- [ ] **Step 5: Wire stage 1 (drift push)**

`src/cli/sync/execute.rs`, in the stage-1 loop over `slug_to_collection` (after the existing `push_dataset` call at :3733-3745), add:

```rust
                    // Row data for manual datasets, gated on its own baseline
                    // so an unchanged data.jsonl costs nothing.
                    if crate::cli::pull::mdh::read_data_mode(&ctx.paths.dataset_dir(slug))?
                        == crate::cli::pull::mdh::DataMode::Manual
                    {
                        let data_path = ctx.paths.dataset_data(slug);
                        let local = std::fs::read(&data_path).ok();
                        let base = ctx
                            .lockfile
                            .objects
                            .get("mdh_data")
                            .and_then(|m| m.get(slug.as_str()))
                            .and_then(|e| e.content_hash.as_deref());
                        let drifted = local.as_deref().map(|b| {
                            crate::state::content_hash(b, &crate::state::Lockfile::default())
                        });
                        if local.is_some() && drifted.as_deref() != base {
                            outcome.items_pushed +=
                                crate::cli::push::mdh_data::push_dataset_data(
                                    &catalog.mdh.client,
                                    ctx.lockfile,
                                    &collection.name,
                                    slug,
                                    ctx.paths,
                                    allow_deletes,
                                    ctx.interactive,
                                    progress,
                                )
                                .await
                                .with_context(|| {
                                    format!("pushing row data for mdh/{slug}")
                                })?;
                        }
                    }
```

Important: this block must run even when `indexes_path` does not exist, so place it BEFORE (or outside) the `if !indexes_path.exists() { continue; }` guard at :3710 — otherwise a dataset that is data-only would never push. Restructure the loop body so the `continue` only skips the index leg.

- [ ] **Step 6: Wire stage 2 (after a create)**

In stage 2, after each successful create path (both the `create_collection` branch that ends with `continue` and the `push_dataset` branch), push the rows. Extract a small closure or repeat the call — the rows must land for a freshly created collection, since that is the whole replication path:

```rust
                    if crate::cli::pull::mdh::read_data_mode(&dataset_dir)?
                        == crate::cli::pull::mdh::DataMode::Manual
                    {
                        outcome.items_pushed += crate::cli::push::mdh_data::push_dataset_data(
                            &catalog.mdh.client,
                            ctx.lockfile,
                            &name,
                            &slug,
                            ctx.paths,
                            allow_deletes,
                            ctx.interactive,
                            progress,
                        )
                        .await
                        .with_context(|| format!("loading row data for new mdh/{slug}"))?;
                    }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --lib cli::sync && cargo test --lib snapshot::refs`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add src/cli/sync/execute.rs src/state/lockfile.rs src/snapshot/refs.rs
git commit -m "feat(mdh): sync pushes, pulls, and prunes manual dataset rows"
```

---

### Task 9: Dry-run forecasts

**Files:**
- Modify: `src/cli/pull/mdh.rs` (`plan_mdh` :109-197, `plan_mdh_index_edits` :237-273)
- Test: inline `mod tests` in `src/cli/pull/mdh.rs`

**Interfaces:**
- Consumes: `read_data_mode` / `DataMode` (Task 3), `to_jsonl` (Task 1), `MdhPlanItem` / `MdhPlanDir` (existing).
- Produces: no new API; `plan_mdh` and `plan_mdh_index_edits` gain data-side items.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/cli/pull/mdh.rs`:

```rust
    /// MDH bypasses the classifier, so `--dry-run` is blind unless the planner
    /// mirrors the executor. Row data must be forecast too: a drifted
    /// data.jsonl is a would-push; a manual dataset with no local rows yet is
    /// a would-pull.
    #[test]
    fn plan_mdh_forecasts_row_data_deltas() {
        use crate::state::{Lockfile, ObjectEntry, content_hash};

        let root = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(root.path(), "dev");
        std::fs::create_dir_all(paths.mdh_dir()).unwrap();

        let ix = b"{\n  \"regular\": [],\n  \"search\": []\n}\n".to_vec();
        // `drifted`: manual, local rows differ from the baseline  -> data push.
        // `fresh`:   manual, no local rows at all                 -> data pull.
        // `plain`:   NOT manual                                   -> no data item.
        for slug in ["drifted", "fresh", "plain"] {
            let dir = paths.dataset_dir(slug);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("indexes.json"), &ix).unwrap();
            let manifest = if slug == "plain" {
                b"{\n  \"name\": \"P\"\n}\n".to_vec()
            } else {
                b"{\n  \"name\": \"M\",\n  \"data\": \"manual\"\n}\n".to_vec()
            };
            std::fs::write(dir.join(COLLECTION_MANIFEST), manifest).unwrap();
        }
        std::fs::write(paths.dataset_data("drifted"), b"{\"code\":\"1000\"}\n").unwrap();

        let entry = |h: &str| ObjectEntry {
            id: 0,
            modified_at: None,
            content_hash: Some(h.to_string()),
            secrets_hash: None,
        };
        let mut lf = Lockfile::default();
        let ix_hash = content_hash(&ix, &Lockfile::default());
        let mut ixs = std::collections::BTreeMap::new();
        for slug in ["drifted", "fresh", "plain"] {
            ixs.insert(slug.to_string(), entry(&ix_hash));
        }
        lf.objects.insert("mdh_indexes".to_string(), ixs);
        let mut data = std::collections::BTreeMap::new();
        data.insert("drifted".to_string(), entry("0000stalehash0000"));
        lf.objects.insert("mdh_data".to_string(), data);

        let collections = ["drifted", "fresh", "plain"]
            .iter()
            .map(|n| Collection { name: n.to_string(), extra: Default::default() })
            .collect();
        let listed = MdhListed {
            client: DataStorageClient::new("http://unused.invalid".to_string(), "t".to_string())
                .unwrap(),
            collections,
            available: true,
        };

        let lines: Vec<String> =
            plan_mdh(&listed, &lf, &paths, false).into_iter().map(|i| i.line).collect();
        assert!(
            lines.contains(&"mdh/drifted data PATCH".to_string()),
            "drifted rows must be forecast as a push: {lines:?}"
        );
        assert!(
            lines.contains(&"mdh/fresh data (new)".to_string()),
            "a manual dataset with no local rows must be forecast as a pull: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("mdh/plain data")),
            "a non-manual dataset must produce no data forecast: {lines:?}"
        );

        // --no-push keeps the pull-side forecast, drops the push-side one.
        let np: Vec<String> =
            plan_mdh(&listed, &lf, &paths, true).into_iter().map(|i| i.line).collect();
        assert!(np.contains(&"mdh/fresh data (new)".to_string()));
        assert!(!np.iter().any(|l| l.contains("data PATCH")));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib cli::pull::mdh::tests::plan_mdh_forecasts_row_data_deltas`
Expected: FAIL — no `data` lines in the plan.

- [ ] **Step 3: Implement in `plan_mdh`**

In `src/cli/pull/mdh.rs`, inside `plan_mdh`: in the `if !no_push` block, after the existing stage-1 loop, add a data-drift loop; and after stage 3, add the data-new loop. `plan_mdh` is infallible (returns `Vec`), so treat a malformed `data` flag as "not manual" here — the real run reports it:

```rust
        // Stage 1b: row-data drift for manual datasets.
        for slug in &remote_slugs {
            if read_data_mode(&paths.dataset_dir(slug)).unwrap_or(DataMode::None)
                != DataMode::Manual
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(paths.dataset_data(slug)) else {
                continue;
            };
            let local_hash = crate::state::content_hash(&bytes, &crate::state::Lockfile::default());
            let base = lockfile
                .objects
                .get("mdh_data")
                .and_then(|m| m.get(slug))
                .and_then(|e| e.content_hash.clone());
            if base.as_deref() != Some(local_hash.as_str()) {
                items.push(MdhPlanItem {
                    dir: MdhPlanDir::Push,
                    line: format!("mdh/{slug} data PATCH"),
                });
            }
        }
```

```rust
    // Stage 3b: a manual dataset with no local row data yet → a pull would
    // create data.jsonl.
    for slug in &remote_slugs {
        if read_data_mode(&paths.dataset_dir(slug)).unwrap_or(DataMode::None) == DataMode::Manual
            && !paths.dataset_data(slug).is_file()
        {
            items.push(MdhPlanItem {
                dir: MdhPlanDir::Pull,
                line: format!("mdh/{slug} data (new)"),
            });
        }
    }
```

- [ ] **Step 4: Extend the network-backed forecast**

In `plan_mdh_index_edits`, after the index-edit item for each collection with a local dir, forecast a row-body pull for manual datasets — reusing `decide_pull_action` so the preview cannot disagree with the real run:

```rust
        if read_data_mode(&paths.dataset_dir(&slug)).unwrap_or(DataMode::None) == DataMode::Manual
        {
            let data_path = paths.dataset_data(&slug);
            if data_path.is_file() {
                let rows = listed.client.find_all(&c.name, Some(progress.clone())).await?;
                let proposed = crate::snapshot::mdh_data::to_jsonl(&rows)?;
                let base = lockfile
                    .objects
                    .get("mdh_data")
                    .and_then(|m| m.get(&slug))
                    .and_then(|e| e.content_hash.clone());
                let (action, _) =
                    decide_pull_action(&data_path, base.as_deref(), &proposed)?;
                if action == PullAction::Write {
                    items.push(MdhPlanItem {
                        dir: MdhPlanDir::Pull,
                        line: format!("mdh/{slug} data (update)"),
                    });
                }
            }
        }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib cli::pull::mdh`
Expected: all pass, including the pre-existing `plan_mdh_previews_all_structural_deltas` (no non-manual dataset gains an item).

- [ ] **Step 6: Commit**

```bash
git add src/cli/pull/mdh.rs
git commit -m "feat(mdh): forecast row-data deltas in sync --dry-run"
```

---

### Task 10: Migrate carries `data.jsonl`

**Files:**
- Modify: `src/cli/migrate/mod.rs:1638-1648` (`is_managed_leaf`)
- Test: inline `mod tests` in `src/cli/migrate/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: no new API.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/cli/migrate/mod.rs`:

```rust
    /// `data.jsonl` is how manual MDH rows travel between envs. Without `jsonl`
    /// as a managed leaf, migrate silently drops it and replication is a no-op.
    #[test]
    fn is_managed_leaf_accepts_jsonl_row_data() {
        assert!(is_managed_leaf("data.jsonl"));
        assert!(is_managed_leaf("collection.json"));
        assert!(is_managed_leaf("extractor.py"));
        assert!(!is_managed_leaf("notes.txt"));
        assert!(!is_managed_leaf("data.jsonl.bak"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib cli::migrate::tests::is_managed_leaf_accepts_jsonl_row_data`
Expected: FAIL on the first assertion.

- [ ] **Step 3: Implement**

`src/cli/migrate/mod.rs`, in `is_managed_leaf`:

```rust
fn is_managed_leaf(name: &str) -> bool {
    matches!(
        name.rsplit_once('.').map(|(_, ext)| ext),
        Some("json") | Some("py") | Some("js") | Some("jsonl")
    )
}
```

Also extend its doc comment: `.jsonl` is MDH row data for a dataset flagged `"data": "manual"`, copied verbatim (it is not `.json`, so it does not go through the parse/substitute path).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib cli::migrate`
Expected: all pass.

- [ ] **Step 5: Add an end-to-end migrate test**

Add to `tests/cli_migrate.rs` (the project fixture is `test` + `prod`, and
`migrate::run(src, tgt, mirror, dry_run, only, migrate_score_thresholds, migrate_email_prefixes)`):

```rust
/// A manual dataset must promote whole: the flag in `collection.json` and the
/// rows in `data.jsonl` both land in the target env, byte-identical. Without
/// `jsonl` as a managed leaf the rows are silently dropped and replication is a
/// no-op that looks like success.
#[test]
fn migrate_carries_manual_mdh_dataset_rows() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");

    write(
        &test_root.join("mdh/gl-codes/collection.json"),
        &serde_json::json!({ "name": "GL_CODES", "data": "manual" }),
    );
    write(
        &test_root.join("mdh/gl-codes/indexes.json"),
        &serde_json::json!({ "regular": [], "search": [] }),
    );
    let rows = "{\"code\":\"1000\",\"label\":\"Office supplies\"}\n\
                {\"code\":\"2000\",\"label\":\"Travel\"}\n";
    std::fs::create_dir_all(test_root.join("mdh/gl-codes")).unwrap();
    std::fs::write(test_root.join("mdh/gl-codes/data.jsonl"), rows).unwrap();

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run("test", "prod", false, false, vec![], true, false);
    std::env::set_current_dir(&prev).unwrap();
    result.expect("migrate should succeed");

    let prod = root.join("envs/prod/mdh/gl-codes");
    assert_eq!(
        std::fs::read_to_string(prod.join("data.jsonl")).unwrap(),
        rows,
        "row data must migrate byte-identically"
    );
    assert_eq!(
        read_json(&prod.join("collection.json"))["data"],
        serde_json::json!("manual"),
        "the opt-in flag must survive the migration"
    );
}
```

Run: `cargo test --test cli_migrate`
Expected: PASS (the whole file stays green — the `--only` integration test lands in Task 11, where the code that makes it pass lands too).

- [ ] **Step 6: Commit**

```bash
git add src/cli/migrate/mod.rs tests/cli_migrate.rs
git commit -m "feat(mdh): migrate carries manual dataset row data"
```

---

### Task 11: S1 — MDH selectable in migrate

Two payoffs: `--only mdh/<slug>` starts working, and a per-env `overlay/mdh/<slug>/data.jsonl` shadow stops aborting the migration (`validate_overlay_dir` rejects any shadow absent from `produced_sidecars`, which derives from `is_sidecar` → `classify_for_selection`).

**Files:**
- Modify: `src/cli/migrate/mod.rs:282-310` (`classify_for_selection`), `src/cli/deploy/selection.rs:17-31` (`DEPLOYABLE_KINDS`), `src/cli/deploy/selection.rs:209-219` (`list_slugs`)
- Test: inline `mod tests` in both files

**Interfaces:**
- Consumes: nothing.
- Produces: `list_slugs(paths, "mdh")` returns dataset slugs.

- [ ] **Step 1: Write the failing tests**

`src/cli/migrate/mod.rs` tests:

```rust
    /// Every leaf of a dataset dir must select as one `("mdh", slug)` object, so
    /// `--only mdh/<slug>` carries the manifest, indexes, and rows together.
    #[test]
    fn classify_for_selection_maps_every_mdh_leaf_to_its_dataset() {
        for leaf in ["collection.json", "indexes.json", "data.jsonl"] {
            assert_eq!(
                classify_for_selection(Path::new(&format!("mdh/gl-codes/{leaf}"))),
                Some(("mdh", "gl-codes".to_string())),
                "{leaf} must select with its dataset"
            );
        }
        // `classify` itself must KEEP returning None for mdh — overlay-key
        // validation and the substitution map depend on that contract.
        assert_eq!(classify(Path::new("mdh/gl-codes/indexes.json")), None);
    }

    /// A per-env row-data shadow must validate as a sidecar, or
    /// `validate_overlay_dir` aborts the whole migration.
    #[test]
    fn is_sidecar_accepts_mdh_row_data() {
        assert!(is_sidecar(Path::new("mdh/gl-codes/data.jsonl")));
        // JSON leaves are still not sidecars (they are overlay.toml territory).
        assert!(!is_sidecar(Path::new("mdh/gl-codes/indexes.json")));
    }
```

`src/cli/deploy/selection.rs` tests (follow the file's existing test style):

```rust
    #[test]
    fn list_slugs_lists_mdh_datasets() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        // A dataset is a dir holding indexes.json — the same definition
        // `local_only_dataset_slugs` uses, so the two never disagree.
        for slug in ["gl-codes", "synonyms"] {
            let dir = paths.dataset_dir(slug);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("indexes.json"), b"{}").unwrap();
        }
        std::fs::create_dir_all(paths.dataset_dir("not-a-dataset")).unwrap();

        assert_eq!(
            list_slugs(&paths, "mdh").unwrap(),
            vec!["gl-codes".to_string(), "synonyms".to_string()]
        );
    }

    #[test]
    fn mdh_is_an_accepted_only_kind() {
        assert!(DEPLOYABLE_KINDS.contains(&"mdh"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib cli::migrate::tests::classify_for_selection_maps_every_mdh_leaf && cargo test --lib cli::deploy::selection`
Expected: FAIL — `None` returned for mdh; `list_slugs` returns an empty vec; `mdh` absent from `DEPLOYABLE_KINDS`.

- [ ] **Step 3: Implement**

`src/cli/migrate/mod.rs`, in `classify_for_selection`'s `match comps.first()`, add an arm (before the catch-all):

```rust
        // Any leaf of a dataset dir selects with its dataset, so `--only
        // mdh/<slug>` carries collection.json + indexes.json + data.jsonl
        // together. `classify` deliberately keeps returning None for mdh.
        Some("mdh") if comps.len() >= 3 => Some(("mdh", comps[1].clone())),
```

`src/cli/deploy/selection.rs`: add `"mdh"` to the end of `DEPLOYABLE_KINDS` (it is last in dependency order — nothing references a dataset), and correct the stale comment above it: MDH index sets and manual row data ARE writable now; only workflows remain pull-only.

Then add the `list_slugs` arm plus its helper:

```rust
        "mdh" => list_mdh_slugs(paths),
```

```rust
/// MDH dataset slugs: directories under `mdh/` holding an `indexes.json`. Same
/// definition `cli::pull::mdh::local_only_dataset_slugs` uses, so selection and
/// the sync executor never disagree about what a dataset is.
fn list_mdh_slugs(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.mdh_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if entry.path().join("indexes.json").exists() {
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    out.sort();
    Ok(out)
}
```

- [ ] **Step 4: Add the end-to-end `--only` test**

Add to `tests/cli_migrate.rs`, beside `migrate_carries_manual_mdh_dataset_rows` from Task 10:

```rust
/// `--only mdh/<slug>` must select the whole dataset — manifest, indexes, and
/// rows — and nothing else. Before S1 this selector matched zero objects and
/// errored out.
#[test]
fn migrate_only_selects_a_whole_mdh_dataset() {
    let project = init_two_env_project();
    let root = project.path();
    let test_root = root.join("envs/test");

    for slug in ["gl-codes", "synonyms"] {
        write(
            &test_root.join(format!("mdh/{slug}/collection.json")),
            &serde_json::json!({ "name": slug, "data": "manual" }),
        );
        write(
            &test_root.join(format!("mdh/{slug}/indexes.json")),
            &serde_json::json!({ "regular": [], "search": [] }),
        );
        std::fs::write(
            test_root.join(format!("mdh/{slug}/data.jsonl")),
            b"{\"code\":\"1000\"}\n",
        )
        .unwrap();
    }

    let _guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let result = rdc::cli::migrate::run(
        "test",
        "prod",
        false,
        false,
        vec!["mdh/gl-codes".into()],
        true,
        false,
    );
    std::env::set_current_dir(&prev).unwrap();
    result.expect("--only mdh/<slug> should succeed");

    let prod = root.join("envs/prod/mdh");
    for leaf in ["collection.json", "indexes.json", "data.jsonl"] {
        assert!(
            prod.join("gl-codes").join(leaf).exists(),
            "{leaf} of the selected dataset must be migrated"
        );
    }
    assert!(
        !prod.join("synonyms").exists(),
        "an unselected dataset must NOT be migrated"
    );
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib cli::migrate && cargo test --lib cli::deploy::selection && cargo test --test cli_migrate`
Expected: all pass, including the new `--only` integration test. If the pre-existing `is_sidecar_matches_only_code_files` test (`:3588`) asserted anything about mdh, update it — the change is intentional.

- [ ] **Step 6: Commit**

```bash
git add src/cli/migrate/mod.rs src/cli/deploy/selection.rs tests/cli_migrate.rs
git commit -m "feat(migrate): make MDH datasets selectable with --only

Also unblocks a per-env overlay/mdh/<slug>/data.jsonl shadow, which
validate_overlay_dir previously rejected because produced_sidecars derives
from classify_for_selection."
```

---

### Task 12: Docs

**Files:**
- Modify: `README.md:105` (tree listing), plus a new subsection under the MDH material near `README.md:365-377`
- Modify: `src/cli/init.rs:812` (the layout listing in the scaffolded project README)
- Test: `cargo test --test cli_init`

**Interfaces:** none.

- [ ] **Step 1: Update the README tree**

`README.md`, in the `envs/test/` tree at :105, extend the mdh entry:

```
└── mdh/                         ← only on clusters with MDH
    └── customers/
        ├── collection.json
        ├── indexes.json
        └── data.jsonl           ← only when "data": "manual"
```

- [ ] **Step 2: Add the feature section**

Add after the existing MDH note (~:377):

````markdown
### Manual MDH datasets

By default rdc versions a dataset's *structure* only — its name and indexes.
Row data belongs to whatever import pipeline feeds it.

A dataset maintained by hand (a synonym list, a GL-code map) is different: its
rows *are* configuration. Opt one in by adding `"data": "manual"` to its
manifest:

```json
{
  "name": "GL_CODES",
  "data": "manual"
}
```

`rdc sync` then pulls the rows to `mdh/<slug>/data.jsonl` — one JSON object per
line, keys and lines sorted so diffs stay readable — and treats that file as
authoritative: after a sync the env holds exactly the rows the file lists.
Deleting rows is gated like every other destructive change (prompt, or
`--allow-deletes`). `rdc migrate` carries the file to the next env, so a change
reviewed in `dev` promotes to `test` and `prod` without retyping.

Notes:

- A row's `_id` is dropped from the file when the server generated it. Give a row
  an explicit `_id` (any string) to pin its identity across environments — edits
  then replace it in place instead of removing and re-adding it.
- rdc warns above 1 000 rows and refuses above 10 000. Import-fed datasets are
  out of scope; rdc warns if a flagged dataset's rows carry the import
  extension's `__digest_md5` marker.
- An absent `data.jsonl` means "not pulled yet", never "delete every row".
- Per-env row overrides work like code sidecars: `envs/<env>/overlay/mdh/<slug>/data.jsonl`.
````

- [ ] **Step 3: Update the scaffolded layout listing**

`src/cli/init.rs:812` — extend the mdh line so a newly-initialized project documents the file:

```
  mdh/<dataset>/                          Master Data Hub (if enabled)
                                          collection.json + indexes.json,
                                          plus data.jsonl when "data": "manual"
```

Match the surrounding column alignment exactly — `tests/cli_init.rs` compares scaffold output byte-for-byte.

- [ ] **Step 4: Run the init test**

Run: `cargo test --test cli_init`
Expected: PASS. If it compares against a golden string, update the golden.

- [ ] **Step 5: Commit**

```bash
git add README.md src/cli/init.rs
git commit -m "docs(mdh): document manual datasets and data.jsonl"
```

---

### Task 13: Live scenario

**Files:**
- Modify: `tests/live/support/mdh.rs` (row helpers), `tests/live/scenarios/mdh.rs` (new scenario)
- Test: `cargo test --test live -- --ignored --test-threads=1` (opt-in, needs `RDC_LIVE_*`)

**Interfaces:**
- Consumes: everything above.
- Produces: `#[tokio::test] async fn live_mdh_manual_data_lifecycle()`.

- [ ] **Step 1: Add row helpers to `MdhRaw`**

`tests/live/support/mdh.rs` already has `insert_one`. Add, in the same style:

```rust
    /// Every row in a collection, via the raw find endpoint.
    pub async fn find_all(&self, name: &str) -> Result<Vec<Value>> {
        let (status, body) = self
            .post("/v1/data/find", json!({ "collectionName": name, "query": {} }))
            .await?;
        if !status.is_success() {
            return Err(anyhow!("find_all {name}: {status} {body}"));
        }
        let env: Value = serde_json::from_str(&body)
            .with_context(|| format!("decoding find_all response for {name}"))?;
        Ok(env
            .get("result")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default())
    }
```

- [ ] **Step 2: Write the scenario**

Add to `tests/live/scenarios/mdh.rs`. It follows `live_mdh_index_lifecycle`'s discipline exactly: per-run throwaway collection, `Teardown` armed before any seeding, `project.run_rdc(...)` to drive the real binary.

```rust
/// Read the run's dataset slug and its row data.
fn read_rows(project: &ProjectFixture, run_id: &RunId) -> (String, Vec<String>) {
    let lf = load_lockfile(project.path(), "test").expect("lockfile");
    let slug = lockfile_keys(&lf, "mdh_indexes")
        .into_iter()
        .find(|s| s.contains(run_id.as_str()))
        .expect("an mdh_indexes slug for this run");
    let rel = format!("envs/test/mdh/{slug}/data.jsonl");
    let body = project.read_to_string(&rel).unwrap_or_default();
    let lines = body.lines().map(str::to_string).collect();
    (slug, lines)
}

/// Flag a dataset manual by adding `"data": "manual"` to its manifest.
fn flag_manual(project: &ProjectFixture, slug: &str) {
    let rel = format!("envs/test/mdh/{slug}/collection.json");
    let mut v = project.read_json(&rel);
    v["data"] = json!("manual");
    let mut bytes = serde_json::to_vec_pretty(&v).unwrap();
    bytes.push(b'\n');
    std::fs::write(project.path().join(&rel), bytes).unwrap();
}

/// Full row-data lifecycle for a MANUAL dataset on a per-run throwaway
/// collection: opt in, pull rows, push an addition, push a deletion, prove
/// idempotence. Never touches a real collection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: needs RDC_LIVE_* env"]
async fn live_mdh_manual_data_lifecycle() {
    let Some(cfg) = LiveConfig::from_env() else {
        eprintln!("{}", LiveConfig::skip_reason());
        return;
    };
    let run_id = RunId::new();
    let coll = mdh_collection_name(&run_id);
    let raw = MdhRaw::connect(&cfg).expect("connect mdh");
    let teardown = Teardown::with_mdh(
        LiveClient::connect(&cfg).expect("connect (teardown)"),
        run_id.clone(),
        cfg.clone(),
    );

    // --- seed remote out-of-band: a collection with two rows ---
    raw.create_collection(&coll).await.expect("create collection");
    raw.insert_one(&coll, json!({ "code": "1000", "label": "Office supplies" }))
        .await
        .expect("seed row 1");
    raw.insert_one(&coll, json!({ "code": "2000", "label": "Travel" }))
        .await
        .expect("seed row 2");

    // --- Phase 1: a NON-flagged dataset must get no data.jsonl at all ---
    let project = ProjectFixture::init(&cfg, &["test", "prod"]).expect("init project");
    let out = project.run_rdc(&["sync", "test", "--no-push"]);
    assert!(out.status.success(), "pull failed: {}", String::from_utf8_lossy(&out.stderr));
    let (slug, rows) = read_rows(&project, &run_id);
    assert!(rows.is_empty(), "an unflagged dataset must have no row data: {rows:?}");

    // --- Phase 2: opt in, pull the rows ---
    flag_manual(&project, &slug);
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "pull after opt-in failed: {}", String::from_utf8_lossy(&out.stderr));
    let (_s, rows) = read_rows(&project, &run_id);
    assert_eq!(
        rows,
        vec![
            r#"{"code":"1000","label":"Office supplies"}"#.to_string(),
            r#"{"code":"2000","label":"Travel"}"#.to_string(),
        ],
        "rows must land canonical: server ids stripped, keys and lines sorted"
    );
    // The opt-in flag must survive the pull that rewrites `name` from the server.
    assert_eq!(
        project.read_json(&format!("envs/test/mdh/{slug}/collection.json"))["data"],
        json!("manual"),
        "the pull must merge collection.json, not clobber it"
    );

    // --- Phase 3: local addition is pushed ---
    let rel = format!("envs/test/mdh/{slug}/data.jsonl");
    let mut body = project.read_to_string(&rel).expect("data.jsonl");
    body.push_str("{\"code\":\"3000\",\"label\":\"Software\"}\n");
    std::fs::write(project.path().join(&rel), &body).unwrap();
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "push-add failed: {}", String::from_utf8_lossy(&out.stderr));
    let remote = raw.find_all(&coll).await.expect("find_all after add");
    assert_eq!(remote.len(), 3, "the added row must reach the env: {remote:?}");

    // --- Phase 4: local deletion is pushed (gated, so pass --allow-deletes) ---
    let kept: String = project
        .read_to_string(&rel)
        .expect("data.jsonl")
        .lines()
        .filter(|l| !l.contains("3000"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(project.path().join(&rel), &kept).unwrap();
    // Without --allow-deletes a non-TTY run must REFUSE.
    let out = project.run_rdc(&["sync", "test"]);
    assert!(
        !out.status.success(),
        "a row deletion must be gated without --allow-deletes"
    );
    assert_eq!(
        raw.find_all(&coll).await.expect("find_all after refusal").len(),
        3,
        "the refused push must not have deleted anything"
    );
    let out = project.run_rdc(&["sync", "test", "--allow-deletes"]);
    assert!(out.status.success(), "gated delete failed: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        raw.find_all(&coll).await.expect("find_all after delete").len(),
        2,
        "the removed row must be deleted on the env"
    );

    // --- Phase 5: idempotence — a re-sync changes nothing, byte for byte ---
    let before = project.read_to_string(&rel).expect("data.jsonl");
    let out = project.run_rdc(&["sync", "test"]);
    assert!(out.status.success(), "re-sync failed: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        project.read_to_string(&rel).expect("data.jsonl"),
        before,
        "an idempotent re-sync must leave data.jsonl byte-identical"
    );
    assert_eq!(
        raw.find_all(&coll).await.expect("find_all after re-sync").len(),
        2,
        "an idempotent re-sync must not touch the rows"
    );

    drop(teardown);
}
```

Note on Phase 4: the exact non-zero-exit behavior comes from `DeleteGate::Bail` (`push/mdh.rs:640-664`) — a non-TTY run without `--allow-deletes` errors out. If the harness runs with a TTY attached, the run would instead prompt; the live harness drives `rdc` with piped stdio, so `Bail` is the path taken.

- [ ] **Step 3: Run it**

```bash
export RDC_LIVE_API_BASE="https://<host>/v1"
export RDC_LIVE_ORG_ID="<org id>"
export RDC_LIVE_TOKEN="<token>"
cargo test --test live -- --ignored --test-threads=1
```
Expected: the new scenario passes alongside the existing ones. It must leave no `rdc_it_*` collection behind.

- [ ] **Step 4: Full suite**

Run: `cargo test`
Expected: green. (`cargo fmt --check` may fail — pre-existing, see Global Constraints.)

- [ ] **Step 5: Commit**

```bash
git add tests/live/support/mdh.rs tests/live/scenarios/mdh.rs
git commit -m "test(mdh): live scenario for manual dataset row data"
```

---

## Spec coverage

| Spec section | Task |
|---|---|
| On-disk format (canonical form, rules 1-6) | 1 |
| `data.jsonl` path | 2 |
| Opt-in flag `"data": "manual"` | 3 |
| Manifest merge (fixes C4) | 3 |
| New API surface (5 verbs, no `bulk_write`) | 4 |
| Diff engine (hybrid keyed diff) | 5 |
| Push: gate, chunking, ordering, `fully_applied`, absent-file rule, ceiling, duplicate-row warning | 6 |
| Pull: guardrail, `__digest_md5` warning, three-way, `mdh_data` kind, `KeepLocal` | 7 |
| Sync stages 1, 2, 3, 4; `mdh_data` sentinel + non-portable | 7 (stage 3), 8 |
| Dry-run forecasts (`plan_mdh`, `plan_mdh_index_edits`) | 9 |
| Migrate integration (`jsonl` leaf, verbatim copy, `--mirror`) | 10 |
| Per-env `overlay/mdh/<slug>/data.jsonl` shadow (C7b) | 11 |
| S1 (`--only`, overlay shadow) | 11 |
| Backward compatibility (zero extra calls when unflagged) | 7 (test), 3 (byte-identity) |
| Failure modes (partial insert, missing collection, over-ceiling, partial push, conflicts) | 4, 6, 7 |
| Testing (hermetic + live) | 1-13 |
