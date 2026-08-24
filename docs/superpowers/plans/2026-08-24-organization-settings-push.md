# Organization `settings` push + promotion — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `envs/<env>/organization.json`'s `settings` subtree a first-class managed surface — pushed by `rdc sync` and promoted by `rdc migrate` — so the document-list columns (`settings.annotation_list_table.columns`) stop being versioned-but-ignored.

**Architecture:** The org becomes a push-capable *singleton* kind: `push::scan` reports it under slug `"self"`, the classifier can then reach `LocalEdit`/`BothDiverged`, and a new driver sends exactly one request — `PATCH /organizations/{id}` with `{"settings": …}`. Everything that makes a kind creatable, deletable or collidable is deliberately left unregistered. Migrate promotes the same subtree by making the org codec's `cross_env_body` retain only `settings`, which hands every other field back to `reconcile_target_identity`.

**Tech Stack:** Rust (edition 2024), `serde_json` with `preserve_order`, `wiremock` + `assert_cmd` for integration tests, `tokio` test runtime.

**Spec:** `docs/superpowers/specs/2026-08-24-organization-settings-push-design.md`

## Global Constraints

- Write scope is **`settings` only**. `ui_settings` and `metadata` are never sent. The 16 read-only fields (`id`, `url`, `name`, `workspaces`, `users`, `rir_key`, `sandbox`, `created_at`, `creator`, `modified_at`, `modified_by`, `internal_info`, `organization_group`, `is_trial`, `expires_at`, `trial_expires_at`, `oidc_provider`) are never sent.
- A `settings` PATCH **replaces** the object server-side — always send a complete `settings`, never a fragment.
- The wire shape of a column is **flat**: `{visible, column_type, width, schema_id, data_type}` for `column_type: "schema"`, `{visible, column_type, width, meta_name}` for `"meta"`. The `{"schema": {…}}` wrapper that `OPTIONS` advertises is rejected by the API with `400 column_type: This field is required.`
- Enum values: `column_type` ∈ `{schema, meta}`; `data_type` ∈ `{string, boolean, date, number}`. `schema_id` `max_length` is **50**. `width` is a float server-side (`120` comes back as `120.0`).
- The API does **not** validate that a `schema_id` exists. An unknown id returns 200.
- **No `LOCKFILE_VERSION` bump. No new `rdc.toml` key. No new CLI flag.**
- rdc must never POST or DELETE an organization. There is no create path and no tombstone path.
- Never put a customer name, org/queue/hook slug, hostname or real `schema_id` in code, tests, fixtures or commit messages. Use `field_a`, `document_id`, `acme`, `main`, `invoices`.
- **Build economy:** this crate is slow to compile. Per task, run only the filtered test (`cargo test --lib <filter>` or `cargo test --test <bin> <filter>`). Run the whole suite once, in Task 7. Never start a rebuild while an integration suite is running — it swaps `target/debug/rdc` under the tests that spawn it.

---

### Task 1: Offline pre-flight validator for org `settings`

Pure function, no wiring. Turns a would-be mid-sync `400` into an offline refusal, and names the OPTIONS wrapper trap explicitly.

**Files:**
- Modify: `src/snapshot/limits.rs` (add type + checker + tests at the end of the existing `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub struct SettingsProblem { pub location: String, pub problem: String }` and `pub fn check_organization_settings(body: &serde_json::Value) -> Vec<SettingsProblem>`. Task 4 calls this from `ChangeList`.

- [ ] **Step 1: Write the failing tests**

Append inside the existing `#[cfg(test)] mod tests` in `src/snapshot/limits.rs`:

```rust
    fn org(cols: serde_json::Value) -> Value {
        serde_json::json!({ "settings": { "annotation_list_table": { "columns": cols } } })
    }

    #[test]
    fn org_settings_clean_body_has_no_problems() {
        let v = org(serde_json::json!([
            { "visible": true, "column_type": "schema", "width": 120.0,
              "schema_id": "document_id", "data_type": "string" },
            { "visible": false, "column_type": "meta", "width": 80.0, "meta_name": "status" },
        ]));
        assert_eq!(check_organization_settings(&v), Vec::new());
    }

    #[test]
    fn org_settings_without_settings_key_is_not_a_problem() {
        // Nothing to validate is not an error — the push driver decides what an
        // absent `settings` means.
        let v = serde_json::json!({ "id": 1, "name": "Acme" });
        assert_eq!(check_organization_settings(&v), Vec::new());
    }

    #[test]
    fn org_settings_rejects_the_options_wrapper_shape() {
        let v = org(serde_json::json!([
            { "schema": { "visible": true, "column_type": "schema", "width": 120.0,
                          "schema_id": "document_id", "data_type": "string" } }
        ]));
        let problems = check_organization_settings(&v);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].location, "settings.annotation_list_table.columns[0]");
        assert!(
            problems[0].problem.contains("flat"),
            "the message must point at the flat form: {}",
            problems[0].problem
        );
    }

    #[test]
    fn org_settings_rejects_unknown_data_type() {
        let v = org(serde_json::json!([
            { "visible": true, "column_type": "schema", "width": 120.0,
              "schema_id": "document_id", "data_type": "bogus" }
        ]));
        let problems = check_organization_settings(&v);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].location, "settings.annotation_list_table.columns[0].data_type");
    }

    #[test]
    fn org_settings_rejects_unknown_column_type() {
        let v = org(serde_json::json!([{ "visible": true, "column_type": "sideways", "width": 1.0 }]));
        let problems = check_organization_settings(&v);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].problem.contains("schema"), "{}", problems[0].problem);
    }

    #[test]
    fn org_settings_reports_each_missing_required_key() {
        let v = org(serde_json::json!([{ "column_type": "schema" }]));
        let problems = check_organization_settings(&v);
        let locs: Vec<&str> = problems.iter().map(|p| p.problem.as_str()).collect();
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(locs.iter().all(|p| p.contains("missing required key")), "{problems:?}");
    }

    #[test]
    fn org_settings_schema_id_at_the_limit_is_accepted_and_one_over_is_not() {
        let at = "a".repeat(50);
        let over = "a".repeat(51);
        let ok = org(serde_json::json!([
            { "visible": true, "column_type": "schema", "width": 1.0, "schema_id": at, "data_type": "string" }
        ]));
        assert_eq!(check_organization_settings(&ok), Vec::new());
        let bad = org(serde_json::json!([
            { "visible": true, "column_type": "schema", "width": 1.0, "schema_id": over, "data_type": "string" }
        ]));
        let problems = check_organization_settings(&bad);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].problem.contains("50"), "{}", problems[0].problem);
    }

    #[test]
    fn org_settings_checks_the_request_dashboard_table_too() {
        let v = serde_json::json!({ "settings": { "request_dashboard_table": { "columns": [
            { "visible": true, "column_type": "schema", "width": 1.0,
              "schema_id": "document_id", "data_type": "nope" }
        ] } } });
        let problems = check_organization_settings(&v);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].location.starts_with("settings.request_dashboard_table"), "{problems:?}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib -- snapshot::limits::tests::org_settings`
Expected: FAIL to compile — `cannot find function check_organization_settings in this scope`.

- [ ] **Step 3: Write the implementation**

Add near `check_rule_actions` in `src/snapshot/limits.rs`:

```rust
/// A structural problem in an organization's `settings`: a wrong enum value, a
/// missing required key, or the polymorphic wrapper shape `OPTIONS` advertises
/// and the API rejects.
///
/// Distinct from [`LimitViolation`], which is only ever about length — most of
/// these have no limit and no measured length to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsProblem {
    /// Where it lives, in terms a human can act on:
    /// `settings.annotation_list_table.columns[0].data_type`.
    pub location: String,
    /// What is wrong, phrased so the message reads as the fix.
    pub problem: String,
}

/// The two column tables under an organization's `settings`. Both take the
/// identical column shape (verified via `OPTIONS /v1/organizations/{id}`).
const ORG_COLUMN_TABLES: &[&str] = &["annotation_list_table", "request_dashboard_table"];
/// `schema_id`'s declared `max_length` on an org column.
const ORG_SCHEMA_ID_LIMIT: usize = 50;
const ORG_DATA_TYPES: &[&str] = &["string", "boolean", "date", "number"];

/// Validate the parts of an organization's `settings` that rdc pushes, against
/// the shape `OPTIONS /v1/organizations/{id}` declares.
///
/// Worth doing offline rather than leaving to the server for one specific
/// reason: `OPTIONS` advertises each column as a polymorphic wrapper
/// (`{"schema": {…}}` / `{"meta": {…}}`) and the API then **rejects** that
/// shape — the accepted body is flat. Anyone reading the API metadata and
/// hand-writing a column hits a `400 column_type: This field is required.`
/// mid-sync; this check names the trap instead.
///
/// A body with no `settings` key produces no problems. What an absent
/// `settings` MEANS is the push driver's decision, not this function's.
pub fn check_organization_settings(body: &Value) -> Vec<SettingsProblem> {
    let mut out = Vec::new();
    let Some(settings) = body.get("settings").and_then(|s| s.as_object()) else {
        return out;
    };
    for table in ORG_COLUMN_TABLES {
        let Some(columns) = settings
            .get(*table)
            .and_then(|t| t.get("columns"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for (i, column) in columns.iter().enumerate() {
            let at = format!("settings.{table}.columns[{i}]");
            let Some(obj) = column.as_object() else {
                out.push(SettingsProblem { location: at, problem: "must be an object".to_string() });
                continue;
            };
            // The wrapper shape OPTIONS advertises and the API refuses.
            if obj.len() == 1 && (obj.contains_key("schema") || obj.contains_key("meta")) {
                out.push(SettingsProblem {
                    location: at,
                    problem: "column is wrapped in a \"schema\"/\"meta\" key; the API wants its \
                              fields flat (OPTIONS advertises the wrapper but rejects it)"
                        .to_string(),
                });
                continue;
            }
            let mut require = |keys: &[&str], out: &mut Vec<SettingsProblem>| {
                for key in keys {
                    if !obj.contains_key(*key) {
                        out.push(SettingsProblem {
                            location: at.clone(),
                            problem: format!("missing required key `{key}`"),
                        });
                    }
                }
            };
            match obj.get("column_type").and_then(|v| v.as_str()) {
                Some("schema") => {
                    require(&["visible", "width", "schema_id", "data_type"], &mut out);
                    if let Some(Value::String(id)) = obj.get("schema_id") {
                        let actual = id.trim().chars().count();
                        if actual > ORG_SCHEMA_ID_LIMIT {
                            out.push(SettingsProblem {
                                location: format!("{at}.schema_id"),
                                problem: format!(
                                    "{actual} characters; the API accepts at most {ORG_SCHEMA_ID_LIMIT}"
                                ),
                            });
                        }
                    }
                    if let Some(Value::String(dt)) = obj.get("data_type")
                        && !ORG_DATA_TYPES.contains(&dt.as_str())
                    {
                        out.push(SettingsProblem {
                            location: format!("{at}.data_type"),
                            problem: format!("`{dt}` is not one of {}", ORG_DATA_TYPES.join(", ")),
                        });
                    }
                }
                Some("meta") => require(&["visible", "width", "meta_name"], &mut out),
                Some(other) => out.push(SettingsProblem {
                    location: format!("{at}.column_type"),
                    problem: format!("`{other}` is not one of schema, meta"),
                }),
                None => out.push(SettingsProblem {
                    location: at.clone(),
                    problem: "missing required key `column_type`".to_string(),
                }),
            }
        }
    }
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib -- snapshot::limits::tests::org_settings`
Expected: PASS, 8 tests.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/limits.rs
git commit -m "feat(limits): validate organization settings columns offline"
```

---

### Task 2: Codec — cross-env body retains only `settings`

This one line is what makes migrate promote `settings` and hand every other field back to the target.

**Files:**
- Modify: `src/snapshot/codec/organization.rs:47-49` (`cross_env_body`) and its `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: `Organization::cross_env_body` now strips every key except `settings`. Task 6 relies on this via `migrate::reconcile_target_identity`.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/snapshot/codec/organization.rs`:

```rust
    #[test]
    fn cross_env_body_retains_only_settings() {
        let mut v = org_value();
        v["ui_settings"] = json!({ "theme": "white" });
        v["metadata"] = json!({ "source": "registration" });
        Organization.cross_env_body(&mut v);
        assert_eq!(
            v,
            json!({ "settings": { "ui_settings": { "language": "en" } } }),
            "cross-env promotion carries `settings` and nothing else: {v}"
        );
    }

    #[test]
    fn cross_env_body_on_a_body_without_settings_yields_an_empty_object() {
        let mut v = json!({ "id": 1, "name": "Acme", "ui_settings": { "theme": "white" } });
        Organization.cross_env_body(&mut v);
        assert_eq!(v, json!({}));
    }
```

Note: `org_value()` already carries `"settings": { "ui_settings": { "language": "en" } }` — that nested `ui_settings` inside `settings` is deliberate in the fixture and must survive; only the TOP-LEVEL `ui_settings` is stripped.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib -- snapshot::codec::organization::tests::cross_env_body`
Expected: FAIL — `create_and_cross_env_bodies_are_noops` style no-op leaves the body unchanged, so the first new test reports the full org body on the left.

- [ ] **Step 3: Write the implementation**

Replace `cross_env_body` in `src/snapshot/codec/organization.rs`:

```rust
    fn cross_env_body(&self, body: &mut Value) {
        // Cross-env promotion carries exactly ONE subtree: `settings`.
        //
        // Everything else on an organization is either read-only at the API
        // (`id`, `url`, `name`, `sandbox`, the stamps, …) or per-env state that
        // must not move between orgs: `ui_settings` holds branding and the
        // org's applied feature flags, `metadata` is free-form. Stripping the
        // rest here is precisely what makes `migrate`'s
        // `reconcile_target_identity` restore those fields from the TARGET's own
        // `organization.json`.
        //
        // A retain-list rather than a strip-list, so a field a future API
        // revision adds defaults to env-local — the conservative direction.
        if let Some(obj) = body.as_object_mut() {
            obj.retain(|k, _| k == "settings");
        }
    }
```

Then update the existing `create_and_cross_env_bodies_are_noops` test, which now asserts stale behavior — split it:

```rust
    #[test]
    fn create_body_is_a_noop() {
        let before = org_value();
        let mut v = before.clone();
        Organization.create_body(&mut v);
        assert_eq!(v, before, "create_body must be a no-op: rdc never POSTs an organization");
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib -- snapshot::codec::organization`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/codec/organization.rs
git commit -m "feat(codec): organization cross-env body carries only settings"
```

---

### Task 3: `[organization]` overlay section

**Files:**
- Modify: `src/overlay.rs` (field, accessor, tests)
- Modify: `src/snapshot/codec/organization.rs` (`overlay()` hook, test)

**Interfaces:**
- Consumes: nothing.
- Produces: `Overlay::organization: BTreeMap<String, Value>`, `Overlay::organization() -> Option<&BTreeMap<String, Value>>`, and `Organization::overlay()` returning it. Task 6 gets overlay application for free through the existing `codec.overlay(...)` call in migrate.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/overlay.rs`:

```rust
    #[test]
    fn organization_section_parses_as_a_nested_table() {
        let toml = r#"
version = 1

[organization.settings.annotation_list_table]
columns = [
  { visible = true, column_type = "meta", width = 120.0, meta_name = "status" },
]
"#;
        let ov: Overlay = toml::from_str(toml).unwrap();
        let org = ov.organization().expect("organization section present");
        let cols = org["settings"]["annotation_list_table"]["columns"]
            .as_array()
            .expect("columns is an array");
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0]["meta_name"], serde_json::json!("status"));
    }

    #[test]
    fn absent_organization_section_is_none() {
        let ov: Overlay = toml::from_str("version = 1\n").unwrap();
        assert!(ov.organization().is_none());
    }

    #[test]
    fn organization_is_not_part_of_kind_maps() {
        // `kind_maps` drives migrate's dangling-key check, which validates every
        // key against a real slug. The org has no slugs, so it must stay out.
        let ov: Overlay = toml::from_str("version = 1\n").unwrap();
        assert_eq!(ov.kind_maps().len(), 9);
        assert!(ov.kind_maps().iter().all(|(k, _)| *k != "organization"));
    }

    #[test]
    fn organization_overlay_replaces_the_columns_array_wholesale() {
        let toml = r#"
version = 1

[organization.settings.annotation_list_table]
columns = [ { visible = true, column_type = "meta", width = 1.0, meta_name = "status" } ]
"#;
        let ov: Overlay = toml::from_str(toml).unwrap();
        let mut value = serde_json::json!({
            "settings": { "annotation_list_table": { "columns": [
                { "visible": false, "column_type": "schema", "width": 9.0,
                  "schema_id": "field_a", "data_type": "string" },
                { "visible": false, "column_type": "schema", "width": 9.0,
                  "schema_id": "field_b", "data_type": "string" },
            ] } },
            "name": "Acme",
        });
        apply_overrides(&mut value, ov.organization().unwrap());
        let cols = value["settings"]["annotation_list_table"]["columns"].as_array().unwrap();
        assert_eq!(cols.len(), 1, "arrays replace wholesale, they do not merge: {value}");
        assert_eq!(cols[0]["meta_name"], serde_json::json!("status"));
        assert_eq!(value["name"], serde_json::json!("Acme"), "untouched keys survive");
    }
```

And in `src/snapshot/codec/organization.rs`:

```rust
    #[test]
    fn overlay_hook_returns_the_organization_section_for_any_slug() {
        let ov: crate::overlay::Overlay = toml::from_str(
            "version = 1\n\n[organization]\nsettings = { annotation_list_table = { columns = [] } }\n",
        )
        .unwrap();
        assert!(Organization.overlay(&ov, "self").is_some());
        assert!(Organization.overlay(&ov, "ignored").is_some(), "slug-independent");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib -- overlay::tests::organization snapshot::codec::organization::tests::overlay_hook`
Expected: FAIL to compile — `no method named organization found for struct Overlay`.

- [ ] **Step 3: Write the implementation**

In `src/overlay.rs`, add the field after `engine_fields` (keep it last so the serialized key order stays stable):

```rust
    /// Organization overrides. The organization is a per-env SINGLETON, so this
    /// is a flat field → value map with no slug layer: `[organization]` in
    /// TOML, or a nested table such as
    /// `[organization.settings.annotation_list_table]`.
    ///
    /// Deliberately NOT part of [`Overlay::kind_maps`]: that list drives
    /// migrate's dangling-key check, which validates each key against a real
    /// slug, and there are no slugs here to validate.
    #[serde(default)]
    pub organization: BTreeMap<String, Value>,
```

and the accessor next to `engine_field`:

```rust
    /// The organization overrides, or `None` when the section is absent or
    /// empty — so a bare `[organization]` header is the same as no header.
    pub fn organization(&self) -> Option<&BTreeMap<String, Value>> {
        (!self.organization.is_empty()).then_some(&self.organization)
    }
```

In `src/snapshot/codec/organization.rs`, replace the `overlay` hook:

```rust
    fn overlay<'a>(
        &self,
        overlay: &'a Overlay,
        _slug: &str,
    ) -> Option<&'a BTreeMap<String, Value>> {
        // One org per env, so the section is slug-independent: `[organization]`,
        // not `[organization.<slug>]`.
        overlay.organization()
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib -- overlay:: snapshot::codec::organization`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/overlay.rs src/snapshot/codec/organization.rs
git commit -m "feat(overlay): add a flat [organization] section"
```

---

### Task 4: Push path, end to end

The org becomes push-capable: scanner → classifier → driver → one PATCH → canonical write-back.

**Files:**
- Modify: `src/api/mod.rs` (add `update_organization` next to `update_label`)
- Create: `src/cli/push/organization.rs`
- Modify: `src/cli/push/mod.rs` (declare the module; dispatch)
- Modify: `src/cli/push/scan.rs` (`ChangeList.organization`, `total`, `is_empty`, the three sweeps, `scan`, `change_list_from_classified`)
- Modify: `src/cli/sync/mod.rs:265-276` (surface the new defect class) and the scan-change insertion block near line 803
- Test: `tests/cli_sync.rs`

**Interfaces:**
- Consumes: `limits::check_organization_settings` (Task 1).
- Produces:
  - `RossumClient::update_organization(&self, id: u64, body: &serde_json::Value, progress: ProgressHandle) -> Result<Organization>`
  - `ChangeList.organization: Option<std::path::PathBuf>` (singleton, so an `Option`, not a map)
  - `ChangeList::organization_settings_problems(&self) -> Vec<(std::path::PathBuf, limits::SettingsProblem)>`
  - `cli::push::organization::push(paths, client, lockfile, path, progress, env) -> Result<(usize, usize)>`

- [ ] **Step 1: Write the failing integration tests**

Add to `tests/cli_sync.rs`. They follow the existing `sync_remote_create_writes_local_organization` shape; the mock records PATCH bodies through a shared `Arc<Mutex<Vec<serde_json::Value>>>` populated from `server.received_requests()`.

```rust
/// A local edit to the org's `settings` produces exactly one
/// `PATCH /organizations/{id}` whose body is `{"settings": …}` and nothing
/// else — no `ui_settings`, no read-only field.
#[tokio::test]
async fn sync_pushes_organization_settings_and_nothing_else() {
    let server = MockServer::start().await;
    let mut org = fixture("organization.json");
    org["settings"] = serde_json::json!({ "annotation_list_table": { "columns": [] } });
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(org.clone()))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(org.clone()))
        .mount(&server)
        .await;
    mock_empty_lists_except(&server, &[]).await;

    let project = TempDir::new().unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    let _cwd_guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    // First sync: pull only, records the base.
    rdc::cli::sync::run("dev", false, false, false, false, false, None)
        .await
        .expect("first sync");

    // Local edit to the managed subtree.
    let org_path = project.path().join("envs/dev/organization.json");
    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&org_path).unwrap()).unwrap();
    on_disk["settings"]["annotation_list_table"]["columns"] = serde_json::json!([
        { "visible": true, "column_type": "meta", "width": 100.0, "meta_name": "status" }
    ]);
    std::fs::write(&org_path, serde_json::to_vec_pretty(&on_disk).unwrap()).unwrap();

    rdc::cli::sync::run("dev", false, false, false, false, false, None)
        .await
        .expect("second sync");
    std::env::set_current_dir(&prev).unwrap();

    let patches: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method == http::Method::PATCH && r.url.path() == "/api/v1/organizations/1")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(patches.len(), 1, "exactly one org PATCH: {patches:?}");
    let body = &patches[0];
    assert_eq!(
        body.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["settings"],
        "the body must carry `settings` and nothing else: {body}"
    );
    assert_eq!(
        body["settings"]["annotation_list_table"]["columns"][0]["meta_name"],
        serde_json::json!("status")
    );
}

/// An edit confined to a field rdc does not manage must not produce a request,
/// and must say so — the write-back would otherwise discard it silently.
#[tokio::test]
async fn sync_warns_and_sends_nothing_for_an_unmanaged_organization_edit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;
    mock_empty_lists_except(&server, &[]).await;

    let project = TempDir::new().unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    let _cwd_guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    rdc::cli::sync::run("dev", false, false, false, false, false, None).await.unwrap();

    let org_path = project.path().join("envs/dev/organization.json");
    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&org_path).unwrap()).unwrap();
    on_disk["ui_settings"] = serde_json::json!({ "theme": "dark" });
    std::fs::write(&org_path, serde_json::to_vec_pretty(&on_disk).unwrap()).unwrap();

    rdc::cli::sync::run("dev", false, false, false, false, false, None).await.unwrap();
    std::env::set_current_dir(&prev).unwrap();

    for req in server.received_requests().await.unwrap_or_default() {
        assert_ne!(
            req.method,
            http::Method::PATCH,
            "an unmanaged-field edit must not PATCH: {} {}",
            req.method,
            req.url.path()
        );
    }
}

/// Deleting `organization.json` must never reach a DELETE. rdc cannot delete an
/// organization and the file is simply re-pulled.
#[tokio::test]
async fn sync_never_deletes_an_organization() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;
    mock_empty_lists_except(&server, &[]).await;

    let project = TempDir::new().unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    let _cwd_guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    rdc::cli::sync::run("dev", false, false, false, false, false, None).await.unwrap();
    std::fs::remove_file(project.path().join("envs/dev/organization.json")).unwrap();
    rdc::cli::sync::run("dev", true, false, true, false, false, None)
        .await
        .expect("sync with --allow-deletes must still succeed");
    std::env::set_current_dir(&prev).unwrap();

    for req in server.received_requests().await.unwrap_or_default() {
        assert_ne!(req.method, http::Method::DELETE, "no DELETE, ever");
    }
}
```

Check `rdc::cli::sync::run`'s parameter order against the existing tests in this file before pasting; the arguments are `(env, interactive, dry_run, allow_deletes, no_push, no_pull, token_override)`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test cli_sync organization`
Expected: FAIL — the first test finds 0 PATCHes (`exactly one org PATCH: []`), because nothing scans or pushes the org yet.

- [ ] **Step 3: Add the API method**

In `src/api/mod.rs`, next to `update_label`:

```rust
    /// `PATCH /organizations/{id}`.
    ///
    /// The only write rdc ever makes to an organization: there is no POST (one
    /// org per env, created outside rdc) and no DELETE. `body` carries exactly
    /// the subtree rdc manages — `{"settings": …}` — because a partial
    /// `settings` PATCH REPLACES the object server-side, so a fragment would
    /// silently drop the sibling keys.
    pub async fn update_organization(
        &self,
        id: u64,
        body: &serde_json::Value,
        progress: ProgressHandle,
    ) -> Result<Organization> {
        self.patch_json(&format!("/organizations/{id}"), body, progress).await
    }
```

- [ ] **Step 4: Add the scanner and the ChangeList surface**

In `src/cli/push/scan.rs`:

```rust
    /// The organization, when `organization.json` differs from its recorded
    /// base. A singleton (lockfile slug `"self"`), so an `Option` rather than a
    /// map — and there is no tombstone counterpart: rdc cannot delete an
    /// organization.
    pub organization: Option<std::path::PathBuf>,
```

`total()` gains `+ usize::from(self.organization.is_some())`.

The three sweeps: `json_parse_errors` and `field_limit_violations` both take
`&BTreeMap`, so wrap the singleton for reuse — add this once, before the
existing `check(...)` calls in `json_parse_errors`:

```rust
        // The org singleton, wrapped so it can go through the same `check`.
        let org_map: BTreeMap<String, std::path::PathBuf> = self
            .organization
            .iter()
            .map(|p| ("self".to_string(), p.clone()))
            .collect();
        check("organization", &org_map);
```

`missing_create_fields` must NOT include the org: it never gets created.

Then the settings checker:

```rust
    /// Structural problems in the organization's `settings` — the subtree push
    /// sends. See [`crate::snapshot::limits::check_organization_settings`] for
    /// why this is worth catching offline.
    pub fn organization_settings_problems(
        &self,
    ) -> Vec<(std::path::PathBuf, crate::snapshot::limits::SettingsProblem)> {
        let Some(path) = &self.organization else {
            return Vec::new();
        };
        let Ok(bytes) = std::fs::read(path) else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Vec::new(); // a parse error is reported by json_parse_errors
        };
        crate::snapshot::limits::check_organization_settings(&value)
            .into_iter()
            .map(|p| (path.clone(), p))
            .collect()
    }
```

The scanner itself:

```rust
/// Hash `organization.json` and report it when it differs from the lockfile
/// base. The org is a singleton (`slug = "self"`) with no create and no delete:
/// a MISSING file is not a tombstone — rdc cannot delete an organization — it
/// simply means there is nothing to push, and the pull half of the same sync
/// writes the file back.
fn scan_organization(
    paths: &Paths,
    lockfile: &Lockfile,
    out: &mut Option<std::path::PathBuf>,
) -> Result<usize> {
    use crate::state::content_hash;
    let path = paths.organization_file();
    if !path.exists() {
        return Ok(0);
    }
    let bytes = std::fs::read(&path)?;
    let local_hash = content_hash(&bytes, &crate::state::Lockfile::default());
    let base_hash = lockfile
        .objects
        .get("organization")
        .and_then(|m| m.get("self"))
        .and_then(|e| e.content_hash.as_deref());
    if base_hash != Some(local_hash.as_str()) {
        *out = Some(path);
    }
    Ok(1)
}
```

Call it from `scan()` after the other kinds:

```rust
    scanned += scan_organization(paths, lockfile, &mut changes.organization)?;
```

And in `change_list_from_classified`, add the arm (LocalEdit only — `LocalCreate`
is unreachable for a singleton that always exists remotely, and a `LocalDelete`
has no push meaning):

```rust
            "organization" => {
                cl.organization = Some(paths.organization_file());
            }
```

- [ ] **Step 5: Write the push driver**

Create `src/cli/push/organization.rs`:

```rust
//! Push driver for the `organization` kind — the only write rdc makes to an
//! organization, and the narrowest one in the codebase.
//!
//! Scope is a single subtree, `settings`. Everything else on the object is
//! read-only at the API or per-env state rdc must not own (`ui_settings` holds
//! branding and the org's applied feature flags; `metadata` is free-form).
//!
//! The body is always a COMPLETE `settings` object, never a fragment: a partial
//! `settings` PATCH replaces the stored object, so sending just one table
//! silently drops its siblings (verified against a live env).
//!
//! No create, no delete: one organization exists per env, made outside rdc.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::state::Lockfile;

/// Top-level keys rdc sends. Everything else in the file is informational.
const MANAGED: &[&str] = &["settings"];

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    path: &Path,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    // The org's id comes from the lockfile: rdc never creates one, so a missing
    // entry means this project has not pulled the org yet. The pull half of the
    // same sync records it; nothing to push this cycle.
    let Some(id) = lockfile
        .objects
        .get("organization")
        .and_then(|m| m.get("self"))
        .map(|e| e.id)
    else {
        progress.event(
            Action::Warn,
            "organization not in the lockfile yet — settings not pushed (pull first)",
        );
        return Ok((0, 1));
    };

    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let local: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;

    // An absent `settings` is NOT the same as an empty one. rdc cannot tell
    // "this project does not manage org settings" from "clear them", and
    // guessing the second wipes the remote — so it does neither. Clearing is
    // written explicitly, as `"settings": {}`.
    let Some(settings) = local.get("settings") else {
        progress.event(
            Action::Warn,
            &format!(
                "{}: no `settings` key — nothing pushed. rdc manages only `settings` on an \
                 organization; write `\"settings\": {{}}` to clear it",
                path.display()
            ),
        );
        return Ok((0, 1));
    };

    // Local edits rdc cannot push would be discarded by the write-back below,
    // which rewrites the file from the server's response. Say so first.
    let unmanaged: Vec<String> = local
        .as_object()
        .map(|o| {
            o.keys()
                .filter(|k| !MANAGED.contains(&k.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !unmanaged.is_empty() {
        // Only interesting when they actually differ from the remote, which the
        // response below reveals; kept as a single line rather than a diff.
        progress.event(
            Action::Info,
            &format!(
                "organization: only `settings` is pushed; {} stay as the env has them",
                unmanaged.join(", ")
            ),
        );
    }

    let body = serde_json::json!({ "settings": settings });
    let updated = client
        .update_organization(id, &body, Some(progress.clone()))
        .await
        .with_context(|| format!("patching organization settings for env '{env}'"))?;

    // Canonical write-back: the same bytes a pull would produce, so the next
    // sync sees `Clean` (this is also what normalizes `width: 120` to the
    // server's `120.0`).
    let value = serde_json::to_value(&updated).context("serializing patched organization")?;
    let art = crate::snapshot::codec::codec("organization")
        .expect("organization codec must exist")
        .disk_bytes(&value)
        .context("serializing organization")?;
    let json = crate::cli::pull::common::portabilize_proposed(&art.json, lockfile);
    let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
    crate::state::base_cache::write_disk_and_cache(paths, path, &json)?;
    crate::cli::pull::common::record_object(
        lockfile,
        "organization",
        "self",
        updated.id,
        updated.modified_at().map(|s| s.to_string()),
        updated.modified_by().map(|s| s.to_string()),
        Some(hash),
    );
    progress.event(Action::Patch, "organization settings");
    Ok((1, 0))
}
```

- [ ] **Step 6: Dispatch it**

In `src/cli/push/mod.rs`, declare `pub mod organization;` alongside the other
drivers, and dispatch last (the org references nothing and nothing references
it, so ordering is free; last keeps it out of the dependency-ordered block):

```rust
    if let Some(path) = &changes.organization {
        tally(
            organization::push(paths, client, lockfile, path, progress, env)
                .await
                .with_context(|| format!("pushing organization for env '{env}'"))?,
        );
    }
```

- [ ] **Step 7: Wire the classifier input and the pre-flight**

In `src/cli/sync/mod.rs`, after the `changes.labels` loop near line 803:

```rust
    // The organization singleton: slug is always "self".
    if let Some(path) = &changes.organization {
        if let Ok(bytes) = std::fs::read(path) {
            let hash = crate::state::content_hash(&bytes, &crate::state::Lockfile::default());
            scan_changes.insert(("organization".to_string(), "self".to_string()), hash);
        }
    }
```

There is deliberately no tombstone counterpart.

At the pre-flight block (line ~266), add the fourth class and pass it through:

```rust
    let settings_problems = changes.organization_settings_problems();
```

Extend `refuse_on_offline_defects` with a `settings_problems: &[(PathBuf, SettingsProblem)]`
parameter, print each as `<path>: <location>: <problem>`, and count it toward the
refusal. Mirror the existing dry-run reporting section for the other three
classes so `--dry-run` prints them too.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test --test cli_sync organization`
Expected: PASS — the three new tests plus the existing org tests.

- [ ] **Step 9: Commit**

```bash
git add src/api/mod.rs src/cli/push/organization.rs src/cli/push/mod.rs \
        src/cli/push/scan.rs src/cli/sync/mod.rs tests/cli_sync.rs
git commit -m "feat(sync): push organization settings"
```

---

### Task 5: Conflict and auto-merge registration

Without this, a `BothDiverged` org (local `settings` edit + any remote change) reaches the resolver with no refs and gets skipped with a warning.

**Files:**
- Modify: `src/cli/sync/execute.rs` (the `match it.kind.as_str()` in the conflict-refs builder, near line 341)
- Test: `tests/cli_sync.rs`

**Interfaces:**
- Consumes: `ChangeList.organization` (Task 4).
- Produces: nothing new; the org now yields `ConflictRefs { hash_strategy: HashStrategy::Flat, .. }`.

- [ ] **Step 1: Write the failing test**

```rust
/// Local `settings` edit + a remote change to a DIFFERENT key is not a
/// conflict: the JSON 3-way merge resolves disjoint keys, so this must sync
/// without a prompt and without losing either side.
#[tokio::test]
async fn sync_auto_merges_disjoint_organization_divergence() {
    let server = MockServer::start().await;
    let base = fixture("organization.json");
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(base.clone()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Second listing: the env changed `ui_settings`, which rdc does not manage.
    let mut remote_changed = base.clone();
    remote_changed["ui_settings"] = serde_json::json!({ "theme": "dark" });
    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(remote_changed.clone()))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(remote_changed.clone()))
        .mount(&server)
        .await;
    mock_empty_lists_except(&server, &[]).await;

    let project = TempDir::new().unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["init", "--env", &format!("dev={}/api/v1:1", server.uri())])
        .assert()
        .success();
    std::fs::write(
        project.path().join("secrets/dev.secrets.json"),
        r#"{"api_token":"TEST_TOKEN"}"#,
    )
    .unwrap();

    let _cwd_guard = cwd_lock();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(project.path()).unwrap();
    rdc::cli::sync::run("dev", false, false, false, false, false, None).await.unwrap();

    let org_path = project.path().join("envs/dev/organization.json");
    let mut on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&org_path).unwrap()).unwrap();
    on_disk["settings"] = serde_json::json!({ "annotation_list_table": { "columns": [
        { "visible": true, "column_type": "meta", "width": 100.0, "meta_name": "status" }
    ] } });
    std::fs::write(&org_path, serde_json::to_vec_pretty(&on_disk).unwrap()).unwrap();

    // Non-interactive: a real conflict would abort or shadow rather than merge.
    rdc::cli::sync::run("dev", false, false, false, false, false, None)
        .await
        .expect("disjoint divergence must auto-merge");
    std::env::set_current_dir(&prev).unwrap();

    assert!(
        !project.path().join(".rdc/conflicts").exists(),
        "disjoint keys must not produce a conflict shadow"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --test cli_sync sync_auto_merges_disjoint_organization`
Expected: FAIL — a conflict shadow is written (or the run warns that the kind has no refs), because the conflict builder has no `"organization"` arm.

- [ ] **Step 3: Add the arm**

In the conflict-refs `match` in `src/cli/sync/execute.rs`, next to `"labels"`:

```rust
            // The organization singleton. Slug is always "self", the path is
            // fixed, and the hash is flat (no sidecars). Pull already writes the
            // base cache for this file, so `try_auto_merge` can resolve
            // divergence on disjoint keys — a local `settings` edit against a
            // remote `ui_settings` change — with no prompt.
            "organization" => {
                let codec = crate::snapshot::codec::codec("organization")?;
                let value = serde_json::to_value(&catalog.organization).ok()?;
                let art = codec.disk_bytes(&value).ok()?;
                Some(ConflictRefs {
                    remote_bytes: art.json,
                    remote_code: None,
                    remote_formulas: Vec::new(),
                    local_path: ctx.paths.organization_file(),
                    id: catalog.organization.id,
                    modified_at: catalog.organization.modified_at().map(|s| s.to_string()),
                    modified_by: catalog.organization.modified_by().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Flat,
                })
            }
```

Check how `catalog` is named and borrowed in this function before pasting; the surrounding arms use pre-built `*_by_slug` maps, which a singleton does not need.

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test --test cli_sync organization`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cli/sync/execute.rs tests/cli_sync.rs
git commit -m "feat(sync): resolve organization conflicts through the 3-way merge"
```

---

### Task 6: Migrate promotion

**Files:**
- Modify: `src/cli/migrate/mod.rs` (`enumerate_files`, `classify`, the per-file guard, `mirror_prune_paths`, the missing-`schema_id` warning)
- Modify: `src/cli/deploy/selection.rs:21` (`DEPLOYABLE_KINDS`)
- Test: `tests/cli_migrate.rs`

**Interfaces:**
- Consumes: `Organization::cross_env_body` (Task 2), `Overlay::organization` (Task 3).
- Produces: nothing new; migrate emits `envs/<tgt>/organization.json`.

- [ ] **Step 1: Write the failing tests**

Add to `tests/cli_migrate.rs`, using its existing `init_two_env_project` / `write` / `read_json` helpers:

```rust
/// Promotion carries `settings` and leaves the target's own identity alone.
#[test]
fn migrate_promotes_organization_settings_only() {
    let project = init_two_env_project();
    let root = project.path();

    write(
        &root.join("envs/test/organization.json"),
        &serde_json::json!({
            "id": 1, "url": "https://test.example/api/v1/organizations/1", "name": "Acme Test",
            "ui_settings": { "theme": "white" },
            "settings": { "annotation_list_table": { "columns": [
                { "visible": true, "column_type": "schema", "width": 120.0,
                  "schema_id": "field_a", "data_type": "string" }
            ] } }
        }),
    );
    write(
        &root.join("envs/prod/organization.json"),
        &serde_json::json!({
            "id": 2, "url": "https://prod.example/api/v1/organizations/2", "name": "Acme Prod",
            "ui_settings": { "theme": "dark" },
            "settings": { "annotation_list_table": { "columns": [] } }
        }),
    );

    let _guard = cwd_lock();
    std::env::set_current_dir(root).unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "test", "prod"])
        .assert()
        .success();

    let tgt = read_json(&root.join("envs/prod/organization.json"));
    assert_eq!(
        tgt["settings"]["annotation_list_table"]["columns"][0]["schema_id"],
        serde_json::json!("field_a"),
        "settings promoted: {tgt}"
    );
    assert_eq!(tgt["id"], serde_json::json!(2), "target identity preserved");
    assert_eq!(tgt["name"], serde_json::json!("Acme Prod"), "target name preserved");
    assert_eq!(
        tgt["ui_settings"]["theme"],
        serde_json::json!("dark"),
        "ui_settings is env-local and must not be promoted"
    );
}

/// No target `organization.json` → skip with a warning, never emit a
/// settings-only file.
#[test]
fn migrate_skips_the_organization_when_the_target_has_none() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/organization.json"),
        &serde_json::json!({ "id": 1, "name": "Acme Test", "settings": {} }),
    );

    let _guard = cwd_lock();
    std::env::set_current_dir(root).unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "test", "prod"])
        .assert()
        .success();

    assert!(
        !root.join("envs/prod/organization.json").exists(),
        "must not create a target org file out of a source-only body"
    );
}

/// `--mirror` must never prune the target's org file, even when the source env
/// has never been pulled.
#[test]
fn migrate_mirror_never_prunes_the_organization() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/prod/organization.json"),
        &serde_json::json!({ "id": 2, "name": "Acme Prod", "settings": {} }),
    );

    let _guard = cwd_lock();
    std::env::set_current_dir(root).unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "test", "prod", "--mirror"])
        .assert()
        .success();

    assert!(
        root.join("envs/prod/organization.json").exists(),
        "a per-env singleton is never a target-only object"
    );
}

/// An `[organization]` overlay entry beats the promoted value.
#[test]
fn migrate_organization_overlay_wins() {
    let project = init_two_env_project();
    let root = project.path();
    write(
        &root.join("envs/test/organization.json"),
        &serde_json::json!({ "id": 1, "name": "Acme Test", "settings": {
            "annotation_list_table": { "columns": [
                { "visible": true, "column_type": "schema", "width": 120.0,
                  "schema_id": "field_a", "data_type": "string" }
            ] } } }),
    );
    write(
        &root.join("envs/prod/organization.json"),
        &serde_json::json!({ "id": 2, "name": "Acme Prod", "settings": {} }),
    );
    std::fs::write(
        root.join("envs/prod/overlay.toml"),
        "version = 1\n\n[organization.settings.annotation_list_table]\n\
         columns = [ { visible = true, column_type = \"meta\", width = 80.0, meta_name = \"status\" } ]\n",
    )
    .unwrap();

    let _guard = cwd_lock();
    std::env::set_current_dir(root).unwrap();
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(root)
        .args(["migrate", "test", "prod"])
        .assert()
        .success();

    let tgt = read_json(&root.join("envs/prod/organization.json"));
    assert_eq!(
        tgt["settings"]["annotation_list_table"]["columns"][0]["meta_name"],
        serde_json::json!("status"),
        "overlay must win over the promoted value: {tgt}"
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --test cli_migrate organization`
Expected: FAIL — the target file is untouched, because `enumerate_files` never yields it.

- [ ] **Step 3: Enumerate and classify the file**

In `src/cli/migrate/mod.rs`, at the end of `enumerate_files`, before the sort:

```rust
    // The organization singleton lives at the env root, outside MANAGED_DIRS, so
    // it is added explicitly rather than by loosening `should_skip` (which also
    // guards `_index.md` / `overlay.toml`, both of which stay excluded).
    if env_root.join("organization.json").exists() {
        out.push(PathBuf::from("organization.json"));
    }
```

In `classify`, add before the `_ => None` arm:

```rust
        Some("organization.json") if comps.len() == 1 => {
            Some(("organization", "self".to_string()))
        }
```

- [ ] **Step 4: Guard the target-missing case and the mirror prune**

In the per-file migration path, immediately after `classify(rel)` resolves the
kind (before any transform), skip the org when the target file is absent:

```rust
    // Promotion writes the source's `settings` into the TARGET's own org object,
    // which requires the target's object to exist: `reconcile_target_identity`
    // restores id/url/name/ui_settings/metadata from it. With no target file
    // there is nothing to restore and the generic path would emit a
    // settings-only organization.json — a snapshot no pull would ever produce.
    if rel == Path::new("organization.json") && !dst_path.exists() {
        warnings.push(format!(
            "envs/{tgt_env}/organization.json does not exist yet — organization settings \
             not promoted; run `rdc sync {tgt_env}` to pull it first"
        ));
        return Ok(FileOutcome::Unchanged);
    }
```

In `mirror_prune_paths`, filter the singleton out of `existing`:

```rust
    let existing = enumerate_files(tgt_root, tgt_env)?;
    Ok(existing
        .into_iter()
        // A per-env singleton is never a "target-only object": the target's org
        // file must survive even when the source env has never been pulled.
        .filter(|rel| rel != Path::new("organization.json"))
        .filter(|rel| !produced.contains(rel))
        .collect())
```

In `src/cli/deploy/selection.rs`, add `"organization"` to `DEPLOYABLE_KINDS` so
`--only organization` is accepted and `--only hooks/*` excludes it.

- [ ] **Step 5: Add the missing-`schema_id` warning**

Still in `src/cli/migrate/mod.rs`, after the overlay is applied to the org body:

```rust
/// Warn for promoted `column_type: "schema"` columns whose `schema_id` appears
/// in no schema under the target env.
///
/// The API accepts an unknown id with a 200 (verified), so the server will never
/// complain — the column just renders empty. Offline is the only place this can
/// surface. Never DROPS the column: silently editing deployable content is worse
/// than a dead column the warning names.
fn org_columns_missing_in_target(value: &Value, tgt_root: &Path) -> Vec<String> {
    let mut known = std::collections::BTreeSet::new();
    for entry in walkdir_json(tgt_root, "schema.json") {
        if let Ok(bytes) = std::fs::read(&entry)
            && let Ok(schema) = serde_json::from_slice::<Value>(&bytes)
        {
            collect_schema_ids(&schema, &mut known);
        }
    }
    let mut missing = Vec::new();
    for table in ["annotation_list_table", "request_dashboard_table"] {
        let Some(cols) = value
            .get("settings")
            .and_then(|s| s.get(table))
            .and_then(|t| t.get("columns"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for col in cols {
            if col.get("column_type").and_then(|v| v.as_str()) == Some("schema")
                && let Some(id) = col.get("schema_id").and_then(|v| v.as_str())
                && !known.contains(id)
            {
                missing.push(id.to_string());
            }
        }
    }
    missing.sort();
    missing.dedup();
    missing
}

/// Every `schema_id` in a schema's `content` tree, at any depth.
fn collect_schema_ids(value: &Value, out: &mut std::collections::BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("id") {
                out.insert(id.clone());
            }
            for v in map.values() {
                collect_schema_ids(v, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_schema_ids(v, out)),
        _ => {}
    }
}
```

`walkdir_json` is the sweep this module already uses to find queue-nested files;
reuse it rather than adding another walker. Push each result onto the same
`warnings` vec the carried-`email_prefix` warning uses, phrased as:
`organization: column schema_id `<id>` does not exist in <tgt_env> — the column will render empty`.

Add a test alongside the others:

```rust
#[test]
fn migrate_warns_about_org_columns_absent_from_the_target_schemas() {
    let project = init_two_env_project();
    let root = project.path();
    write(&root.join("envs/test/organization.json"), &serde_json::json!({
        "id": 1, "name": "Acme Test", "settings": { "annotation_list_table": { "columns": [
            { "visible": true, "column_type": "schema", "width": 1.0,
              "schema_id": "not_in_prod", "data_type": "string" },
            { "visible": true, "column_type": "meta", "width": 1.0, "meta_name": "status" }
        ] } }
    }));
    write(&root.join("envs/prod/organization.json"),
          &serde_json::json!({ "id": 2, "name": "Acme Prod", "settings": {} }));
    write(&root.join("envs/prod/workspaces/main/queues/invoices/schema.json"),
          &serde_json::json!({ "name": "s", "content": [ { "id": "in_prod", "category": "datapoint" } ] }));

    let _guard = cwd_lock();
    std::env::set_current_dir(root).unwrap();
    let out = assert_cmd::Command::cargo_bin("rdc").unwrap()
        .current_dir(root).args(["migrate", "test", "prod"]).assert().success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let all = format!("{stdout}{stderr}");
    assert!(all.contains("not_in_prod"), "must name the missing schema_id: {all}");
    assert!(!all.contains("status"), "a meta column has no schema_id to check: {all}");
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --test cli_migrate organization`
Expected: PASS, 5 tests.

- [ ] **Step 7: Commit**

```bash
git add src/cli/migrate/mod.rs src/cli/deploy/selection.rs tests/cli_migrate.rs
git commit -m "feat(migrate): promote organization settings between envs"
```

---

### Task 7: Docs, live coverage, full-suite verification

**Files:**
- Modify: `README.md` (a subsection under the sync docs)
- Modify: `tests/live/` (one opt-in scenario, following the existing harness pattern)

**Interfaces:**
- Consumes: everything above.
- Produces: nothing.

- [ ] **Step 1: Document the surface in README.md**

Add after the snapshot-layout section, keeping every env-derived line out of it
(no markers needed — this text is not generated):

```markdown
### Organization settings

`envs/<env>/organization.json` holds the org object. rdc manages exactly one
subtree of it: **`settings`** — which is where the document-list columns live
(`settings.annotation_list_table.columns`, and the request dashboard's
equivalent). Edit them there and `rdc sync <env>` sends them:

    PATCH /v1/organizations/<id>   { "settings": … }

Everything else in the file is informational: `ui_settings` (branding, theme,
the org's applied feature flags), `metadata`, and the read-only fields the API
assigns. Editing those locally changes nothing remotely, and the next sync
rewrites them from the env.

A `settings` PATCH replaces the whole object server-side, so rdc always sends
the complete subtree. That has one consequence worth knowing: **an absent
`settings` key means "not managed", not "empty"** — rdc skips the push and says
so. To clear the org's settings, write `"settings": {}` explicitly.

`rdc migrate` promotes `settings` and nothing else, into the target's own org
object. Override per env with an `[organization]` overlay:

```toml
# envs/prod/overlay.toml
[organization.settings.annotation_list_table]
columns = [
  { visible = true, column_type = "meta", width = 120.0, meta_name = "status" },
]
```

A column's `schema_id` is a per-env schema field id and the API does not check
that it exists, so migrate warns when a promoted column names a field the target
has no schema for — the column would render empty.
```

- [ ] **Step 2: Add the opt-in live scenario**

Follow the existing scenarios in `tests/live/`: GET the org, PATCH
`settings.annotation_list_table.columns` to a one-column list built from a
`meta_name` (`"status"` — needs no schema field, so it is valid in any env), GET
to confirm it persisted, then restore the captured original `settings` verbatim
and assert the restore matches. Gate it behind the same env var the other live
tests use, and assert on `settings` only — never on `name` or any org identifier.

- [ ] **Step 3: Run the full suite once**

Run: `cargo test --workspace`
Expected: PASS, 0 failed. This is the only full-suite run in the plan; everything
before it was filtered.

- [ ] **Step 4: Check the desktop bridge still compiles**

Run: `cd desktop/rust && cargo check`
Expected: exit 0. It is a separate workspace with a path dependency on `rdc`, so
a changed public type there breaks it silently otherwise.

- [ ] **Step 5: Commit**

```bash
git add README.md tests/live
git commit -m "docs: document organization settings management"
```

---

## Self-review

**Spec coverage.** §A push contract → Task 4 (driver, absent-`settings` rule, unmanaged-field notice, write-back). §B sync wiring → Task 4 (scanner, `ChangeList`, `change_list_from_classified`, dispatch, `scan_changes`) and Task 5 (conflict refs). §C pre-flight → Task 1 (checker) and Task 4 Step 7 (call site + refusal). §D migrate → Task 2 (`cross_env_body`), Task 3 (overlay), Task 6 (enumerate/classify/skip/prune/`--only`/`schema_id` warning). §E idempotency → Task 4's canonical write-back, asserted by the second sync being `Clean` in the Task 4 test. Backward compatibility → no lockfile/`rdc.toml`/flag changes anywhere in the plan; overlay additivity covered by Task 3's tests. Testing → Tasks 1–7. Non-goals → nothing in any task touches `ui_settings`, `metadata`, create or delete.

**Types.** `SettingsProblem { location, problem }` is defined in Task 1 and consumed with those exact field names in Task 4 Step 4. `ChangeList.organization: Option<PathBuf>` is declared in Task 4 Step 4 and read in Steps 4, 6, 7. `update_organization(id, &Value, ProgressHandle) -> Result<Organization>` is defined in Task 4 Step 3 and called in Step 5. `record_object` is called with seven arguments, matching its current signature.

**Two places the implementer must read before pasting**, both flagged inline: `rdc::cli::sync::run`'s argument order (Task 4 Step 1) and how `catalog` is borrowed in the conflict-refs builder (Task 5 Step 3).
