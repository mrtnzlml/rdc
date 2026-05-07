# rdc M9 — Schema Combined Hash + Formula `.py` Three-Way Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the M5-documented gap. Schemas now use a *combined* SHA-256 covering `schema.json` AND every `formulas/<id>.py` file, so local edits to formula code are detected on subsequent pulls. On schema conflict, emit BOTH `schema.json.remote` AND `formulas.remote/<id>.py` files so the user sees the full remote schema. After M9, formula edits are first-class in the merge story.

**Architecture:** A new `schema_combined_hash(json_bytes, &[(field_id, py_bytes)]) -> String` helper in `state::lockfile` implements the M5-documented algorithm: `SHA-256(json || 0x00 || "formulas/<id>.py" || 0x00 || py_bytes || ...)` with formulas sorted by `field_id`. Schema codec splits into `serialize_schema(model) -> (json_bytes, formulas_map)` (no I/O) and a thin `write_schema` wrapper. The queues driver's schema section uses the combined hash for 3-way: read local schema.json + walk `formulas/*.py` to compute local combined hash; compute remote combined hash from serialize_schema output; compare both to lockfile's content_hash. On conflict: preserve local `schema.json` + `formulas/*.py` AND emit `schema.json.remote` + `formulas.remote/<id>.py`.

**Tech Stack:** Same as M8.

**Scope:**
- ✅ Combined-hash 3-way for schemas (JSON + formulas)
- ✅ Conflict emits remote schema.json + formula files
- ❌ NOT cross-reference indexer (deferred to a future milestone — small enhancement, not blocking)
- ❌ NOT `rdc resolve` (the .remote file workflow is sufficient)

**End state of M9:**

```
$ # First pull: schema with one formula
$ rdc pull dev
$ ls envs/dev/workspaces/invoices-ap/queues/cost-invoices/
queue.json  schema.json  inbox.json  formulas/

$ # Edit the formula locally
$ vim envs/dev/workspaces/invoices-ap/queues/cost-invoices/formulas/amount_total.py

$ rdc pull dev   # remote unchanged → local kept
# (no conflict)

$ # Now someone changes the SAME formula on the server
$ rdc pull dev
warning: ... schema.json conflict — local preserved, remote at schema.json.remote
Pulled ..., 1 conflict from env 'dev'

$ ls envs/dev/workspaces/invoices-ap/queues/cost-invoices/
queue.json  schema.json  schema.json.remote  inbox.json  formulas/  formulas.remote/

$ ls envs/dev/workspaces/invoices-ap/queues/cost-invoices/formulas.remote/
amount_total.py
```

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `src/state/lockfile.rs` | Modify | Add `schema_combined_hash` function |
| `src/snapshot/schema.rs` | Modify | Add `serialize_schema` (no I/O); add `read_local_formulas`; refactor `write_schema` to call `serialize_schema` |
| `src/cli/pull/queues.rs` | Modify | Use combined hash for schema 3-way; on conflict emit `formulas.remote/` |
| `tests/cli_pull.rs` | Modify | New tests covering formula 3-way scenarios |
| `README.md` | Modify | Note M9 closes the schema hash gap |

---

## Task 1: `schema_combined_hash` helper

**Files:**
- Modify: `src/state/lockfile.rs`

- [ ] **Step 1: Add the function + tests**

Append to `src/state/lockfile.rs` (after the existing `content_hash` function, before the `#[cfg(test)] mod tests` block):

```rust
/// Compute a stable SHA-256 over a schema's combined content: the
/// post-extraction `schema.json` bytes plus each formula file (path + body).
/// Formulas must be passed sorted by `field_id` for determinism.
///
/// The algorithm matches the documentation on `write_schema` in
/// `src/snapshot/schema.rs`:
///
/// ```text
/// SHA-256(
///     json_bytes
///     || 0x00 || "formulas/<id>.py" || 0x00 || formula_bytes
///     || ...   (continued for every formula, in field_id order)
/// )
/// ```
pub fn schema_combined_hash(json_bytes: &[u8], formulas: &[(String, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(json_bytes);
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

- [ ] **Step 2: Add unit tests**

Inside `mod tests`, append:

```rust
    #[test]
    fn schema_combined_hash_no_formulas() {
        let h1 = schema_combined_hash(b"{}", &[]);
        let h2 = schema_combined_hash(b"{}", &[]);
        assert_eq!(h1, h2);
        // With no formulas, must equal plain content_hash of the JSON.
        assert_eq!(h1, content_hash(b"{}"));
    }

    #[test]
    fn schema_combined_hash_with_formulas_is_deterministic() {
        let formulas = vec![
            ("amount_total".to_string(), b"a + b".to_vec()),
            ("invoice_id".to_string(), b"x".to_vec()),
        ];
        let h1 = schema_combined_hash(b"{}", &formulas);
        let h2 = schema_combined_hash(b"{}", &formulas);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn schema_combined_hash_changes_when_formula_changes() {
        let json = b"{}";
        let f1 = vec![("amount_total".to_string(), b"a + b".to_vec())];
        let f2 = vec![("amount_total".to_string(), b"a + b + c".to_vec())];
        assert_ne!(
            schema_combined_hash(json, &f1),
            schema_combined_hash(json, &f2)
        );
    }

    #[test]
    fn schema_combined_hash_changes_when_field_id_changes() {
        let json = b"{}";
        let f1 = vec![("amount_total".to_string(), b"x".to_vec())];
        let f2 = vec![("amount_due".to_string(), b"x".to_vec())];
        assert_ne!(
            schema_combined_hash(json, &f1),
            schema_combined_hash(json, &f2)
        );
    }
```

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib state::lockfile`
Expected: 11 tests pass (7 from M8 + 4 new).

- [ ] **Step 4: Commit**

```bash
git add src/state/lockfile.rs
git commit -m "feat(state): schema_combined_hash function (json + formula files)"
```

---

## Task 2: `serialize_schema` + `read_local_formulas` helpers

**Files:**
- Modify: `src/snapshot/schema.rs`

The current `write_schema` does extraction internally; we need a way to get `(json_bytes, formulas_map)` *without* writing. Refactor by extracting the extraction logic into a reusable function.

- [ ] **Step 1: Add `serialize_schema` and `read_local_formulas`**

Append the following to `src/snapshot/schema.rs` (after the existing `read_schema` function, before the `extract_formulas` private function):

```rust
/// Serialize a schema to its on-disk byte form WITHOUT writing. Returns the
/// JSON bytes (post-extraction) and the list of `(field_id, formula_bytes)`
/// pairs sorted by field_id. Used by the queues driver to compute
/// `schema_combined_hash` for 3-way merge before deciding whether to write.
pub fn serialize_schema(schema: &Schema) -> Result<(Vec<u8>, Vec<(String, Vec<u8>)>)> {
    let mut value = serde_json::to_value(schema)
        .context("serializing schema to value")?;

    let mut formulas: Vec<(String, String)> = Vec::new();
    if let Some(content) = value.get_mut("content").and_then(|c| c.as_array_mut()) {
        for node in content.iter_mut() {
            extract_formulas(node, &mut formulas);
        }
    }
    formulas.sort_by(|a, b| a.0.cmp(&b.0));

    let bytes = serde_json::to_vec_pretty(&value)
        .context("serializing schema json")?;
    let mut bytes = bytes;
    bytes.push(b'\n');

    let formulas_bytes: Vec<(String, Vec<u8>)> = formulas
        .into_iter()
        .map(|(id, code)| (id, code.into_bytes()))
        .collect();

    Ok((bytes, formulas_bytes))
}

/// Walk the on-disk `<queue_dir>/formulas/` directory and return
/// `(field_id, bytes)` pairs sorted by `field_id`. Returns an empty vec if the
/// directory does not exist. Used to compute the LOCAL combined hash.
pub fn read_local_formulas(queue_dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let formulas_dir = queue_dir.join("formulas");
    if !formulas_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in std::fs::read_dir(&formulas_dir)
        .with_context(|| format!("reading {}", formulas_dir.display()))?
    {
        let entry = entry
            .with_context(|| format!("listing {}", formulas_dir.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(field_id) = name.strip_suffix(".py") {
            let bytes = std::fs::read(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            out.push((field_id.to_string(), bytes));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
```

- [ ] **Step 2: Add tests**

Inside the existing `mod tests` block, append:

```rust
    #[test]
    fn serialize_schema_returns_json_and_formulas() {
        let s = sample_with_formula();
        let (json_bytes, formulas) = serialize_schema(&s).unwrap();
        // JSON should not contain the formula text (it was extracted).
        let json_str = std::str::from_utf8(&json_bytes).unwrap();
        assert!(!json_str.contains("amount_due + amount_tax"));
        // formulas should contain exactly one entry for amount_total.
        assert_eq!(formulas.len(), 1);
        assert_eq!(formulas[0].0, "amount_total");
        assert_eq!(formulas[0].1, b"amount_due + amount_tax".to_vec());
    }

    #[test]
    fn serialize_schema_returns_empty_formulas_when_none() {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/schemas/1",
            "name": "S",
            "queues": [],
            "content": [{ "category": "datapoint", "id": "f", "type": "string" }]
        });
        let s: Schema = serde_json::from_value(v).unwrap();
        let (_, formulas) = serialize_schema(&s).unwrap();
        assert!(formulas.is_empty());
    }

    #[test]
    fn read_local_formulas_returns_empty_when_dir_missing() {
        let dir = TempDir::new().unwrap();
        let res = read_local_formulas(dir.path()).unwrap();
        assert!(res.is_empty());
    }

    #[test]
    fn read_local_formulas_returns_sorted_by_field_id() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("formulas")).unwrap();
        std::fs::write(dir.path().join("formulas/zeta.py"), b"z").unwrap();
        std::fs::write(dir.path().join("formulas/alpha.py"), b"a").unwrap();
        std::fs::write(dir.path().join("formulas/mid.py"), b"m").unwrap();
        let f = read_local_formulas(dir.path()).unwrap();
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].0, "alpha");
        assert_eq!(f[1].0, "mid");
        assert_eq!(f[2].0, "zeta");
    }
```

- [ ] **Step 3: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test --lib snapshot::schema`
Expected: schema tests pass — 5 from M3 + 4 new = 9.

- [ ] **Step 4: Commit**

```bash
git add src/snapshot/schema.rs
git commit -m "feat(snapshot): serialize_schema + read_local_formulas helpers"
```

---

## Task 3: Use combined hash in queues driver schema 3-way

**Files:**
- Modify: `src/cli/pull/queues.rs`

The current schema 3-way only hashes `schema.json` bytes. Switch to combined hash, and on conflict emit a `formulas.remote/` directory alongside `schema.json.remote`.

- [ ] **Step 1: Update the schema section in `queues.rs`**

In `src/cli/pull/queues.rs`, locate the comment `// 2. schema.json — three-way (formula .py files always overwritten in M8)` and replace the entire schema block (from that comment up to the `record_object(... "schemas" ...)` call inclusive) with:

```rust
        // 2. schema (json + formulas) — three-way using combined hash (M9)
        let schema_id = parse_id_from_url(&q.schema)
            .with_context(|| format!("parsing schema URL '{}' for queue '{}'", q.schema, q.name))?;
        let schema = ctx
            .client
            .get_schema(schema_id)
            .await
            .with_context(|| format!("fetching schema {schema_id} for queue '{}'", q.name))?;

        // Compute the LOCAL combined hash from disk (if files exist).
        let schema_path = queue_dir.join("schema.json");
        let pre_local_json = if schema_path.exists() {
            Some(std::fs::read(&schema_path)
                .with_context(|| format!("reading {}", schema_path.display()))?)
        } else {
            None
        };
        let pre_local_formulas = crate::snapshot::schema::read_local_formulas(&queue_dir)?;

        // Compute the REMOTE proposed bytes + formulas (without writing yet).
        let (remote_json_bytes, remote_formulas) =
            crate::snapshot::schema::serialize_schema(&schema)?;
        let remote_combined_hash =
            crate::state::schema_combined_hash(&remote_json_bytes, &remote_formulas);

        // Decide.
        let schema_base = ctx
            .lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get(&q_slug))
            .and_then(|e| e.content_hash.clone());
        let s_action = match (schema_base.as_deref(), &pre_local_json) {
            (None, _) => PullAction::Write,
            (_, None) => PullAction::Write,
            (Some(base), Some(local_json)) => {
                let local_combined_hash =
                    crate::state::schema_combined_hash(local_json, &pre_local_formulas);
                let local_matches = local_combined_hash == base;
                let remote_matches = remote_combined_hash == base;
                match (local_matches, remote_matches) {
                    (true, _) => PullAction::Write,
                    (false, true) => PullAction::KeepLocal,
                    (false, false) => PullAction::Conflict,
                }
            }
        };

        let schema_recorded = match s_action {
            PullAction::Write => {
                // Write json + each formula file. Codec already does this; reuse it.
                crate::snapshot::schema::write_schema(&queue_dir, &schema)
                    .with_context(|| format!("writing schema for queue '{}'", q.name))?;
                remote_combined_hash
            }
            PullAction::KeepLocal => {
                // Don't touch disk. Recompute local hash for the lockfile (so it
                // reflects the canonical local combined state).
                let local_json = pre_local_json.as_ref().unwrap();
                crate::state::schema_combined_hash(local_json, &pre_local_formulas)
            }
            PullAction::Conflict => {
                // Local stays. Emit `schema.json.remote` and a sibling
                // `formulas.remote/<id>.py` directory with the remote formulas.
                let remote_path = queue_dir.join("schema.json.remote");
                crate::snapshot::writer::write_atomic(&remote_path, &remote_json_bytes)?;
                if !remote_formulas.is_empty() {
                    let remote_formulas_dir = queue_dir.join("formulas.remote");
                    std::fs::create_dir_all(&remote_formulas_dir)
                        .with_context(|| format!("creating {}", remote_formulas_dir.display()))?;
                    for (field_id, bytes) in &remote_formulas {
                        let p = remote_formulas_dir.join(format!("{field_id}.py"));
                        crate::snapshot::writer::write_atomic(&p, bytes)?;
                    }
                }
                eprintln!(
                    "warning: {} conflict — local preserved, remote at {} (formulas at {})",
                    schema_path.display(),
                    queue_dir.join("schema.json.remote").display(),
                    queue_dir.join("formulas.remote").display()
                );
                counts.conflicts += 1;
                let local_json = pre_local_json.as_ref().unwrap();
                crate::state::schema_combined_hash(local_json, &pre_local_formulas)
            }
        };
        record_object(
            ctx.lockfile,
            "schemas",
            &q_slug,
            schema.id,
            Some(schema.url.clone()),
            schema.modified_at().map(|s| s.to_string()),
            Some(schema_recorded),
        );
        counts.schemas += 1;
```

This:
- Replaces the M8 always-write-then-restore pattern with a "compute decision first, write only on Write" pattern (closer to the flat-list driver pattern).
- Uses `serialize_schema` to get proposed bytes WITHOUT writing.
- Writes BOTH JSON and formula files only when the decision is `Write`.
- On conflict, emits both the JSON .remote and a `formulas.remote/` dir.

- [ ] **Step 2: Run tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all 145 tests still pass (existing fixtures: first pull → all Write actions, no behavior change).

- [ ] **Step 3: Commit**

```bash
git add src/cli/pull/queues.rs
git commit -m "feat(cli): schema 3-way uses combined hash (json + formulas)"
```

---

## Task 4: Integration tests for formula three-way

**Files:**
- Modify: `tests/cli_pull.rs`

- [ ] **Step 1: Add formula-edit integration tests**

Append to `tests/cli_pull.rs`:

```rust
/// Editing a formula `.py` file locally and re-pulling with unchanged remote
/// must preserve the local edit (no overwrite).
#[tokio::test]
async fn re_pull_preserves_local_formula_edit_when_remote_unchanged() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/workspaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/queues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_1.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/201"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_2.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/202"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_3.json")))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/inboxes/300"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("inbox_1.json")))
        .mount(&server).await;
    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });
    for ep in [
        "/api/v1/hooks", "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
        "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
    ] {
        Mock::given(method("GET"))
            .and(path(ep))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
            .mount(&server).await;
    }

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "dev"]).assert().success();

    // Edit the formula .py file locally.
    let formula_path = project.path().join("envs/dev/workspaces/invoices-ap/queues/cost-invoices/formulas/amount_total.py");
    let original = std::fs::read_to_string(&formula_path).unwrap();
    let edited = format!("{original} + 0  # local tweak");
    std::fs::write(&formula_path, &edited).unwrap();

    // Re-pull with same fixture → KeepLocal expected.
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("conflict").not());

    let after = std::fs::read_to_string(&formula_path).unwrap();
    assert_eq!(after, edited, "local formula edit must be preserved");
}

/// Editing a formula locally AND on the remote causes a conflict; both
/// schema.json.remote and formulas.remote/<id>.py are emitted.
#[tokio::test]
async fn re_pull_emits_remote_files_on_formula_conflict() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    let modified_schema = serde_json::json!({
        "id": 200,
        "url": "https://mock.rossum.app/api/v1/schemas/200",
        "name": "Cost Invoices Schema",
        "queues": ["https://mock.rossum.app/api/v1/queues/100"],
        "content": [
            {
                "category": "section",
                "id": "header",
                "label": "Header",
                "children": [
                    { "category": "datapoint", "id": "invoice_id", "type": "string" },
                    {
                        "category": "datapoint",
                        "id": "amount_total",
                        "type": "number",
                        "formula": "amount_due + amount_tax + REMOTE_FORMULA_EDIT"
                    }
                ]
            }
        ],
        "modified_at": "2026-04-10T09:00:00Z"
    });

    let empty = serde_json::json!({ "pagination": { "next": null }, "results": [] });

    for srv in [&server1, &server2] {
        Mock::given(method("GET"))
            .and(path("/api/v1/organizations/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/workspaces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("workspaces_list.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/queues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("queues_list.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/schemas/201"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_2.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/schemas/202"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_3.json")))
            .mount(srv).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/inboxes/300"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("inbox_1.json")))
            .mount(srv).await;
        for ep in [
            "/api/v1/hooks", "/api/v1/rules", "/api/v1/labels", "/api/v1/engines", "/api/v1/engine_fields",
            "/api/v1/workflows", "/api/v1/workflow_steps", "/api/v1/email_templates",
        ] {
            Mock::given(method("GET"))
                .and(path(ep))
                .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
                .mount(srv).await;
        }
    }

    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("schema_1.json")))
        .mount(&server1).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/schemas/200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(modified_schema))
        .mount(&server2).await;

    let project = TempDir::new().unwrap();
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--name", "x", "--env", &format!("dev={}/api/v1:1", server1.uri())])
        .assert().success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    ).unwrap();

    Command::cargo_bin("rdc").unwrap().current_dir(project.path()).args(["pull", "dev"]).assert().success();

    // Edit local formula
    let formula_path = project.path().join("envs/dev/workspaces/invoices-ap/queues/cost-invoices/formulas/amount_total.py");
    let local_edit = "LOCAL_FORMULA_EDIT".to_string();
    std::fs::write(&formula_path, &local_edit).unwrap();

    // Repoint to server2 (remote modified the same formula)
    let cfg_path = project.path().join("rdc.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    let new_cfg = cfg.replace(&format!("{}/api/v1", server1.uri()), &format!("{}/api/v1", server2.uri()));
    std::fs::write(&cfg_path, new_cfg).unwrap();

    // Pull → conflict expected
    Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["pull", "dev"])
        .assert().success()
        .stdout(predicate::str::contains("1 conflict"));

    // Local formula preserved
    let after = std::fs::read_to_string(&formula_path).unwrap();
    assert_eq!(after, local_edit);

    // Remote artifacts emitted
    let queue_dir = project.path().join("envs/dev/workspaces/invoices-ap/queues/cost-invoices");
    assert!(queue_dir.join("schema.json.remote").exists());
    assert!(queue_dir.join("formulas.remote/amount_total.py").exists());
    let remote_formula = std::fs::read_to_string(queue_dir.join("formulas.remote/amount_total.py")).unwrap();
    assert!(remote_formula.contains("REMOTE_FORMULA_EDIT"));
}
```

- [ ] **Step 2: Run all tests**

Run: `. "$HOME/.cargo/env" && cargo test`
Expected: all tests pass — adds 2 new cli_pull tests.

- [ ] **Step 3: Commit**

```bash
git add tests/cli_pull.rs
git commit -m "test(cli): integration tests for formula three-way"
```

---

## Task 5: README update

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update**

In `README.md`, update the conflict-handling section to remove the formulas-always-overwritten caveat:

Replace the line:
```
schemas (JSON only — formula `.py` files are always overwritten until the
combined-hash work in M9), inboxes,
```

With:
```
schemas (combined hash covers schema.json + every formula `.py` file), inboxes,
```

Also update the Status line:
```
**Status:** M9. Pull side feature-complete with three-way conflict detection across all kinds (M7, M8, M9), per-env `_index.md` generation, and formula-aware schema hashes. See `docs/superpowers/specs/2026-05-06-rdc-design.md` for the full design.
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: M9 closes the schema combined-hash gap"
```

---

## Self-Review

**Spec coverage:**
- §8.2 Algorithm semantic merging on schema content arrays — partially: hashes detect conflicts at JSON level, semantic field-level merging still future work
- M5-documented combined-hash algorithm — implemented exactly per spec

**Placeholder scan:** No "TBD"/"TODO" patterns.

**Type consistency:** `schema_combined_hash(json_bytes, &[(String, Vec<u8>)]) -> String` consistent in Tasks 1, 3. `serialize_schema(&Schema) -> Result<(Vec<u8>, Vec<(String, Vec<u8>)>)>` consistent in Tasks 2, 3. `read_local_formulas(&Path) -> Result<Vec<(String, Vec<u8>)>>` consistent.

**Scope check:** 5 tasks. The novel piece is the combined hash algorithm + multi-file conflict emission; everything else follows established patterns.

---

## Next milestones

- **M10:** `rdc push` — local snapshot back to remote with two-phase send + verify. The biggest user-facing milestone.
- **M11:** Overlays.
- **M12:** Mapping wizard, `rdc plan`, `rdc apply`.
- **M13:** Auxiliary commands (status, diff, auth, repair); cross-reference indexer.
- **M14:** Distribution.
