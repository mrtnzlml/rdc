# Field-Limit Pre-Flight: Hoist and Coverage — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rdc sync`'s over-length field check run before any network call, and extend it to the sidecar-backed and nested authored-text fields it currently misses.

**Architecture:** Two independent changes to an existing, working feature. First, a reordering inside `sync::run_cycle` so the purely-local scan and its validations run before token resolution and the remote listing. Second, new checks in `src/snapshot/limits.rs` for values that live in `.py` sidecars (`rules.trigger_condition`, schema `formula`) or nested inside the JSON body (schema `prompt` / `memory.index_formula`, `rules actions[].payload.content`), wired into the existing `ChangeList::field_limit_violations()`. `doctor` inherits every new check for free because it calls the same function.

**Tech Stack:** Rust 2024, `serde_json`, `anyhow`, `wiremock` + `assert_cmd` for integration tests.

**Spec:** `docs/superpowers/specs/2026-08-12-field-limit-preflight-hoist-and-coverage-design.md`

## Global Constraints

- **Never over-report.** A missed violation degrades to today's behavior (the server rejects it). A false positive blocks a legitimate push, which is strictly worse. When uncertain, do not flag.
- **Count Unicode code points after trimming both ends** — `s.trim().chars().count()`. The server trims surrounding whitespace before validating, verified live.
- **Never run repo-wide `cargo fmt`.** This repo is not fmt-clean under the local rustfmt; a repo-wide format would produce a huge unrelated diff. Format only what you write, by hand.
- **Commit to local `main`. Never `git push`.** The maintainer publishes himself.
- **No customer names or customer-specific identifiers** anywhere — source, tests, fixtures, or commit messages. Use neutral placeholders (`acme`, `main`, `invoices`, `dev`/`test`/`prod`).
- Verified limits, do not change these numbers: `rules.trigger_condition` 4000, schema `formula` 2000, schema `prompt` 5000, schema `memory.index_formula` 2000, `rules actions[].payload.content` 4096.
- Full suite must stay green: `cargo test` (lib ~896 + integration ~219). A cold build takes ~13 minutes; budget for it.

---

### Task 1: Hoist the offline pre-flight above all network work

The user-visible defect: an error knowable from local bytes alone is reported only after `resolve_token` (which can perform a network login and write `secrets/<env>.secrets.json`) and after `list_remote` fetches 13 endpoints.

**Files:**
- Modify: `src/cli/sync/mod.rs` (in `run_cycle`, roughly lines 220–320 and the two refusal blocks around 545–590)
- Test: `tests/cli_sync.rs`

**Interfaces:**
- Consumes: `crate::cli::push::scan::scan(&Paths, &Lockfile) -> Result<(usize, ChangeList, Tombstones)>`, `ChangeList::json_parse_errors()`, `ChangeList::field_limit_violations()` — all already exist and are unchanged by this task.
- Produces: a private helper `fn refuse_on_offline_defects(parse_errors: &[JsonParseError], limit_violations: &[FieldLimitViolation]) -> anyhow::Result<()>` in `src/cli/sync/mod.rs`, and the reordered `run_cycle` body. No public API change.

- [ ] **Step 1: Write the failing test**

Add to `tests/cli_sync.rs`. This asserts the *behavior change* (zero network) rather than restating the existing refusal.

```rust
/// An over-length field is knowable from local bytes alone. `sync` must
/// refuse without issuing a single request — no token resolution, no
/// remote listing. Before the pre-flight was hoisted this scenario cost
/// 13 list calls before failing.
#[tokio::test]
async fn sync_refuses_oversized_field_before_any_network_call() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/organizations/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("organization.json")))
        .mount(&server)
        .await;

    let rules_body = serde_json::json!({
        "pagination": { "total": 1, "total_pages": 1, "next": null, "previous": null },
        "results": [{
            "id": 2597,
            "url": format!("{}/api/v1/rules/2597", server.uri()),
            "name": "Example Rule",
            "description": "short",
            "queues": [],
            "modified_at": "2026-04-20T08:00:00Z"
        }]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/rules"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rules_body))
        .mount(&server)
        .await;
    mock_empty_lists_except(&server, &["/api/v1/rules"]).await;

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

    // Pull once so the lockfile has a base for the rule.
    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--no-push", "--yes"])
        .assert()
        .success();

    // Lengthen `description` past the 255-char cap.
    let json_path = project.path().join("envs/dev/rules/example-rule.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    v.as_object_mut().unwrap().insert(
        "description".to_string(),
        serde_json::Value::String("x".repeat(300)),
    );
    std::fs::write(&json_path, serde_json::to_string_pretty(&v).unwrap()).unwrap();

    let before = server.received_requests().await.unwrap_or_default().len();

    assert_cmd::Command::cargo_bin("rdc")
        .unwrap()
        .current_dir(project.path())
        .args(["sync", "dev", "--yes"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("the API allows 255"));

    let after = server.received_requests().await.unwrap_or_default().len();
    assert_eq!(
        before, after,
        "sync must refuse an over-length field without issuing ANY request; \
         it made {} call(s) before failing",
        after - before
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test cli_sync sync_refuses_oversized_field_before_any_network_call -- --nocapture`

Expected: FAIL on the `assert_eq!(before, after)` — the sync currently issues ~13 list calls before refusing. The `stderr` assertion passes even before the fix; only the request-count assertion should fail.

- [ ] **Step 3: Extract the refusal into a helper**

In `src/cli/sync/mod.rs`, add this private function near the bottom of the module (before `#[cfg(test)] mod tests`, if present). The two message bodies are copied **verbatim** from the existing refusal blocks so output does not change.

```rust
/// Refuse a push over defects that are knowable from local bytes alone.
///
/// Both classes are *permanent*: an unparseable file and an over-length
/// field can never be accepted by the server, so attempting the push
/// aborts the cycle before the pull phase on every single run — wedging
/// the project until a human notices. Raising them here keeps the remote
/// untouched and names exactly what to fix.
fn refuse_on_offline_defects(
    parse_errors: &[crate::cli::push::scan::JsonParseError],
    limit_violations: &[crate::cli::push::scan::FieldLimitViolation],
) -> Result<()> {
    use std::fmt::Write as _;

    if !parse_errors.is_empty() {
        let mut msg = format!(
            "{} changed local file(s) are not valid JSON; refusing to push before any remote write:",
            parse_errors.len()
        );
        for e in parse_errors {
            let _ = write!(
                msg,
                "\n  - {}/{} -- {}: {}",
                e.kind,
                e.slug,
                e.path.display(),
                e.error
            );
        }
        anyhow::bail!("{msg}");
    }

    if !limit_violations.is_empty() {
        let mut msg = format!(
            "{} changed local field(s) exceed the Rossum API's length limit; \
             refusing to push before any remote write:",
            limit_violations.len()
        );
        for v in limit_violations {
            let _ = write!(
                msg,
                "\n  - {}/{} -- {}: {} is {} characters, the API allows {} \
                 (shorten it by {})",
                v.kind,
                v.slug,
                v.path.display(),
                v.field,
                v.actual,
                v.limit,
                v.actual.saturating_sub(v.limit),
            );
        }
        anyhow::bail!("{msg}");
    }

    Ok(())
}
```

- [ ] **Step 4: Reorder `run_cycle`**

In `run_cycle`, the current order is: token → client → lockfile → renderer → `list_remote` → `scan` → classify → validations. Change it to: lockfile → renderer → `scan` + validations + refusal → token → client → `list_remote` → classify.

Move the renderer construction up as well, so a pre-flight failure renders through the same `Log` and the output format is unchanged.

Concretely, delete these lines from their current position (they currently sit immediately after `env_cfg` is resolved):

```rust
    let token = match token_override {
        Some(t) => t,
        None => resolve_token(&cwd, env, &env_cfg.api_base).await?,
    };
    let client = RossumClient::new(env_cfg.api_base.clone(), token.clone())
        .context("constructing Rossum API client")?;
```

and re-insert them **after** the new pre-flight block. The resulting sequence, starting from the existing `let mut lockfile = Lockfile::load(...)` line:

```rust
    let mut lockfile = Lockfile::load(&paths.lockfile())?;
    // Set the env's api_base so the lockfile can DERIVE object URLs from
    // ids (push ref resolution, deploy cross-ref rewriting). Without this an
    // empty api_base makes `url_for_slug` return None and push refs fail loud.
    lockfile.api_base = env_cfg.api_base.clone();

    let _title = if dry_run {
        format!("rdc sync {env} (dry run)")
    } else {
        format!("rdc sync {env}")
    };
    let renderer_was_supplied = renderer.is_some();
    let progress: Arc<Log> =
        renderer.unwrap_or_else(|| Log::new(crate::cli::resolve::detect_color_mode()));
    let started = std::time::Instant::now();

    // Phase 0: offline pre-flight. Scanning the local tree needs nothing
    // but the lockfile, and both validations below are decidable from
    // local bytes — so they run before the token is resolved and before a
    // single remote call. `resolve_token` can perform a network login and
    // rewrite `secrets/<env>.secrets.json`; there is no reason to pay for
    // that on a cycle that cannot proceed.
    //
    // The scan result is reused by the classify phase below, so the tree
    // is still walked and hashed exactly once.
    let (_scanned, changes, tombstones) = crate::cli::push::scan::scan(&paths, &lockfile)?;
    let parse_errors = changes.json_parse_errors();
    let limit_violations = changes.field_limit_violations();

    // `--no-push` is an audit mode: there is nothing to half-apply, so it
    // proceeds and merely reports. `--dry-run` proceeds too — its job is
    // to print the COMPLETE plan, and it already surfaces both classes in
    // dedicated sections further down.
    if !no_push && !dry_run {
        refuse_on_offline_defects(&parse_errors, &limit_violations)?;
    }

    let token = match token_override {
        Some(t) => t,
        None => resolve_token(&cwd, env, &env_cfg.api_base).await?,
    };
    let client = RossumClient::new(env_cfg.api_base.clone(), token.clone())
        .context("constructing Rossum API client")?;

    // Phase 1: list remote. Mirrors `pull::run`'s `PullCtx` construction
    // verbatim so the listing semantics are identical.
    let catalog = {
        let mut ctx = crate::cli::pull::common::PullCtx {
            paths: &paths,
            client: &client,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive,
        };
        crate::cli::pull::common::list_remote(&mut ctx, env_cfg, env, &token, &progress).await?
    };
```

Then delete the now-duplicated statements that followed `list_remote`:

```rust
    let (_scanned, changes, tombstones) = crate::cli::push::scan::scan(&paths, &lockfile)?;
```
```rust
    let parse_errors = changes.json_parse_errors();
```
```rust
    let limit_violations = changes.field_limit_violations();
```

Leave the `detect_slug_collisions` warning loop and the `from_catalog_scan_lockfile` classify call exactly where they are — they still read `changes`/`tombstones`, which are now bound earlier.

Finally, delete the two old refusal blocks that sat after the dry-run early return (the `if !no_push && !parse_errors.is_empty() { ... }` block and the `if !no_push && !limit_violations.is_empty() { ... }` block), since `refuse_on_offline_defects` now covers both. Keep the dry-run reporting sections (`progress.event(Action::Plan, "parse errors")` and `"field limit errors"`) untouched.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --test cli_sync sync_refuses_oversized_field_before_any_network_call -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Verify nothing else regressed**

Run: `cargo test --test cli_sync && cargo test --test cli_doctor && cargo test --lib`
Expected: all green. Pay attention to any test asserting on the ORDER of log lines in a sync — the `list` lines now come after the pre-flight.

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src/cli/sync/mod.rs tests/cli_sync.rs
git commit -m "fix(sync): run the offline pre-flight before token resolution and listing

An over-length field or an unparseable local file is decidable from local
bytes alone, but sync only reported it after resolving the token and
listing all 13 remote endpoints. Hoist the local scan and both validations
above that work and refuse there.

resolve_token can perform a network login and rewrite the secrets file, so
this also stops a cycle that cannot proceed from touching credentials. The
scan result is reused by the classify phase, so the tree is still walked and
hashed exactly once -- this is a reordering, not extra work.

Note a visible change: when a project has BOTH an over-length field and a
broken token, the field error now wins. That is the actionable one and it
needs no credentials.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Count length the way the server counts it

The server trims surrounding whitespace before validating — verified with a single request carrying one 2001-char formula and one 2000-char-plus-newline formula, where only the first errored. rdc writes code sidecars without a trailing newline but most editors add one, so counting raw would reject values the server accepts.

**Files:**
- Modify: `src/snapshot/limits.rs:98-113` (`check_field_limits`)
- Test: `src/snapshot/limits.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces: no new API — only the counting rule inside `check_field_limits` changes. The shared `check_text` primitive that later tasks reuse arrives in Task 3, because it returns a `LimitViolation` whose `field` must be a `String` and that type change is Task 3's job.

Keep the tests in this task written against the current `field: &'static str` shape (`field: "description"`, no `.to_string()`); Task 3 updates them along with the type.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/snapshot/limits.rs`:

```rust
    /// The server trims surrounding whitespace before validating: a value
    /// at exactly the limit plus a trailing newline is ACCEPTED. Verified
    /// live with a single request carrying a 2001-char formula and a
    /// 2000-char-plus-newline formula — only the first errored. Counting
    /// raw would reject every sidecar an editor added a final newline to.
    #[test]
    fn trailing_newline_does_not_count_toward_the_limit() {
        let body = json!({ "description": format!("{}\n", "x".repeat(2000)) });
        assert_eq!(check_field_limits("hooks", &body), vec![]);
    }

    /// Trimming applies to both ends and to whitespace generally, not just
    /// a newline. Trimming at least as much as the server is the safe side:
    /// under-report and the server still rejects; over-report and a valid
    /// push is blocked.
    #[test]
    fn surrounding_whitespace_does_not_count_toward_the_limit() {
        let body = json!({ "description": format!("  {}\t\n", "x".repeat(2000)) });
        assert_eq!(check_field_limits("hooks", &body), vec![]);
    }

    /// The reported length is what the server sees, so an over-limit value
    /// reports its TRIMMED length — otherwise "shorten it by N" is wrong.
    #[test]
    fn reported_length_is_the_trimmed_length() {
        let body = json!({ "description": format!("\n{}\n", "x".repeat(2500)) });
        assert_eq!(
            check_field_limits("hooks", &body),
            vec![LimitViolation { field: "description", limit: 2000, actual: 2500 }],
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib snapshot::limits`
Expected: the three new tests FAIL (a 2001-char raw value is reported as a violation; the third reports `actual: 2502`).

- [ ] **Step 3: Trim before counting**

In `src/snapshot/limits.rs`, in `check_field_limits`, change:

```rust
        let actual = s.chars().count();
```

to:

```rust
        // The server trims surrounding whitespace before validating, so
        // count what it will actually measure. Counting raw would reject a
        // value the server accepts — for the very common case of an editor
        // adding a final newline.
        let actual = s.trim().chars().count();
```

Then update the module doc comment at the top of the file: after the sentence explaining that limits count Unicode code points, add:

```rust
//! Values are counted **after trimming surrounding whitespace**, which is
//! what the server does before validating (verified live: a value at
//! exactly the limit plus a trailing newline is accepted).
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib snapshot::limits`
Expected: PASS, including the pre-existing boundary tests (`hook_description_exactly_at_limit_is_accepted`, `hook_description_one_over_limit_is_reported`) which must be unaffected.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/limits.rs
git commit -m "fix(limits): count field length the way the server does

The API trims surrounding whitespace before validating -- verified with a
single request carrying a 2001-char formula and a 2000-char-plus-newline
formula, where only the first errored. rdc writes code sidecars without a
trailing newline but most editors add one, so counting raw bytes would
reject values the server accepts.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Let a violation carry a dynamic location

`LimitViolation.field` is `&'static str`, which can only name a top-level key. The sidecar and nested checks in Tasks 4–7 need labels built at runtime — `formula on datapoint 'total_amount'`, `actions[1] (show_message) payload.content`.

**Files:**
- Modify: `src/snapshot/limits.rs` (the `LimitViolation` struct, `check_field_limits`, and every inline test constructing a `LimitViolation`)
- Modify: `src/cli/push/scan.rs:141-151` (`FieldLimitViolation`) and `:101-135` (`field_limit_violations`)
- Test: `src/snapshot/limits.rs` inline tests (updated, not new)

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `pub struct LimitViolation { pub field: String, pub limit: usize, pub actual: usize }`
  - `pub struct FieldLimitViolation { pub kind: &'static str, pub slug: String, pub path: PathBuf, pub field: String, pub limit: usize, pub actual: usize }`
  - `pub fn check_text(field: impl Into<String>, limit: usize, text: &str) -> Option<LimitViolation>`

`kind` stays `&'static str` — every kind is a compile-time constant. Only `field` becomes dynamic.

- [ ] **Step 1: Change the two struct definitions**

In `src/snapshot/limits.rs`:

```rust
/// One field whose local value is longer than the API accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitViolation {
    /// Where the offending value lives, in terms a human can act on: a
    /// top-level JSON key (`description`), or a built-up location for a
    /// nested or sidecar value (`formula on datapoint 'total_amount'`).
    /// Dynamic because the server's own error for nested fields is
    /// positional and carries no id — naming the object is the whole
    /// value this check adds over the raw 400.
    pub field: String,
    /// The API's declared `max_length` for this field.
    pub limit: usize,
    /// The local value's length, in the same unit the server counts.
    pub actual: usize,
}
```

In `src/cli/push/scan.rs`, change `pub field: &'static str,` to `pub field: String,` in `FieldLimitViolation`, keeping the surrounding doc comments.

- [ ] **Step 2: Add the shared counting primitive**

In `src/snapshot/limits.rs`, add below `check_field_limits`:

```rust
/// Check one string against one limit, returning a violation if it is too
/// long. The single place length is measured — every check in this module
/// funnels through it, so the trimming rule cannot drift between them.
pub fn check_text(field: impl Into<String>, limit: usize, text: &str) -> Option<LimitViolation> {
    let actual = text.trim().chars().count();
    (actual > limit).then(|| LimitViolation { field: field.into(), limit, actual })
}
```

Rewrite the body of `check_field_limits` to use it:

```rust
    let mut out = Vec::new();
    for (field, limit) in field_limits(kind) {
        let Some(Value::String(s)) = obj.get(*field) else {
            continue;
        };
        if let Some(v) = check_text(*field, *limit, s) {
            out.push(v);
        }
    }
    out
```

- [ ] **Step 3: Update the call site and the tests**

In `src/cli/push/scan.rs`, inside `field_limit_violations`, the struct literal now moves the owned label:

```rust
                for v in crate::snapshot::limits::check_field_limits(kind, &body) {
                    out.push(FieldLimitViolation {
                        kind,
                        slug: slug.clone(),
                        path: path.clone(),
                        field: v.field,
                        limit: v.limit,
                        actual: v.actual,
                    });
                }
```

In the inline tests in `src/snapshot/limits.rs`, every `field: "description"` becomes `field: "description".to_string()` (and likewise `"name"`). The format strings in `src/cli/sync/mod.rs` and `src/cli/doctor/mod.rs` need no change — `String` and `&str` both render through `{}`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib snapshot::limits && cargo test --lib cli::push::scan`
Expected: PASS, with identical assertions to before modulo `.to_string()`.

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/limits.rs src/cli/push/scan.rs
git commit -m "refactor(limits): let a violation carry a dynamic location

Nested and sidecar-backed values need a label built at runtime (\"formula on
datapoint 'total_amount'\"), which a &'static str cannot express. Also adds
check_text as the single place length is measured, so the trimming rule
cannot drift between checks.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Validate `rules.trigger_condition` (dead table entry → real check)

`field_limits("rules")` lists `("trigger_condition", 4000)`, but the rules codec extracts that field into a `<slug>.py` sidecar and `src/snapshot/codec/rules.rs` has a test (`trigger_condition_not_in_json`) asserting it is never in the JSON. `check_field_limits` only reads top-level JSON keys, so the entry can never fire.

**Files:**
- Modify: `src/snapshot/limits.rs` (`field_limits`, plus a new constant)
- Modify: `src/cli/push/scan.rs` (`field_limit_violations`)
- Test: `src/cli/push/scan.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `check_text` from Task 3.
- Produces: `pub const RULE_TRIGGER_CONDITION_LIMIT: usize = 4000;` in `src/snapshot/limits.rs`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/cli/push/scan.rs`. Match the fixture style of the neighbouring `field_limit_violations_*` tests.

```rust
    /// `trigger_condition` lives in `<slug>.py`, never in the rule JSON
    /// (see `snapshot::codec::rules`), so a JSON-only check can never see
    /// it. The server enforces 4000 characters and rejects anything longer
    /// with a permanent 400.
    #[test]
    fn field_limit_violations_reports_oversized_rule_trigger_condition() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("my-rule.json"),
            br#"{"name":"My Rule","queues":[]}"#,
        )
        .unwrap();
        std::fs::write(rules_dir.join("my-rule.py"), "x".repeat(4001).as_bytes()).unwrap();

        let mut cl = ChangeList::default();
        cl.rules
            .insert("my-rule".to_string(), rules_dir.join("my-rule.json"));

        let v = cl.field_limit_violations();
        assert_eq!(v.len(), 1, "expected exactly one violation, got {v:?}");
        assert_eq!(v[0].kind, "rules");
        assert_eq!(v[0].field, "trigger_condition");
        assert_eq!(v[0].limit, 4000);
        assert_eq!(v[0].actual, 4001);
        assert_eq!(
            v[0].path,
            rules_dir.join("my-rule.py"),
            "the violation must point at the .py sidecar the user edits, not the JSON"
        );
    }

    /// Boundary: exactly at the limit passes, and so does the limit plus a
    /// trailing newline — rdc writes sidecars without one but editors add
    /// it, and the server trims before validating.
    #[test]
    fn rule_trigger_condition_at_limit_and_with_trailing_newline_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("my-rule.json"),
            br#"{"name":"My Rule","queues":[]}"#,
        )
        .unwrap();
        std::fs::write(
            rules_dir.join("my-rule.py"),
            format!("{}\n", "x".repeat(4000)).as_bytes(),
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.rules
            .insert("my-rule".to_string(), rules_dir.join("my-rule.json"));

        assert_eq!(cl.field_limit_violations().len(), 0);
    }

    /// A rule with no `trigger_condition` has no sidecar at all; the check
    /// must not treat a missing file as an error.
    #[test]
    fn rule_without_trigger_condition_sidecar_is_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("my-rule.json"),
            br#"{"name":"My Rule","queues":[]}"#,
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.rules
            .insert("my-rule".to_string(), rules_dir.join("my-rule.json"));

        assert_eq!(cl.field_limit_violations().len(), 0);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cli::push::scan::tests::field_limit_violations_reports_oversized_rule_trigger_condition`
Expected: FAIL — `expected exactly one violation, got []`.

- [ ] **Step 3: Move the limit out of the JSON table and check the sidecar**

In `src/snapshot/limits.rs`, remove `("trigger_condition", 4000)` from the `rules` arm and add the constant above `field_limits`:

```rust
/// `rules.trigger_condition` is capped at 4000 characters, but it is never
/// present in the rule JSON: the codec extracts it into a `<slug>.py`
/// sidecar (see `snapshot::codec::rules`). It therefore cannot live in
/// [`field_limits`], which only inspects top-level JSON keys — an entry
/// there is silently dead. `ChangeList::field_limit_violations` reads the
/// sidecar and checks it against this constant instead.
pub const RULE_TRIGGER_CONDITION_LIMIT: usize = 4000;
```

Change the `rules` arm to:

```rust
        // A rule's `description` cap is 255 — far tighter than a hook's 2000.
        // `trigger_condition` is NOT here on purpose: see
        // `RULE_TRIGGER_CONDITION_LIMIT`.
        "rules" => &[("name", 255), ("description", 255)],
```

In `src/cli/push/scan.rs`, inside `field_limit_violations`, after the generic per-kind loop that calls `check`, add:

```rust
        // `rules.trigger_condition` lives in `<slug>.py`, never in the JSON,
        // so the JSON walk above can never see it. Point the violation at
        // the sidecar — that is the file the user opens to fix it.
        for (slug, json_path) in &self.rules {
            let py_path = json_path.with_extension("py");
            let Ok(text) = std::fs::read_to_string(&py_path) else {
                continue; // no trigger_condition, or unreadable — push surfaces I/O errors
            };
            if let Some(v) = crate::snapshot::limits::check_text(
                "trigger_condition",
                crate::snapshot::limits::RULE_TRIGGER_CONDITION_LIMIT,
                &text,
            ) {
                out.push(FieldLimitViolation {
                    kind: "rules",
                    slug: slug.clone(),
                    path: py_path,
                    field: v.field,
                    limit: v.limit,
                    actual: v.actual,
                });
            }
        }
```

Note the borrow: the existing `check` closure borrows `out` mutably, so this loop must come **after** the closure's last use. If the compiler objects, wrap the closure's calls in a block or drop it with `drop(check);` before this loop.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib cli::push::scan && cargo test --lib snapshot::limits`
Expected: PASS. The existing `same_field_has_different_limit_per_kind` test still passes — it asserts on `description`, not `trigger_condition`.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/limits.rs src/cli/push/scan.rs
git commit -m "fix(limits): actually validate rules.trigger_condition

The 4000-char entry was in the limits table but could never fire: the codec
extracts trigger_condition into a <slug>.py sidecar (there is a test
asserting it is absent from the JSON) while the checker only reads top-level
JSON keys. Read the sidecar and point the violation at it, since that is the
file a human edits.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Validate schema formulas (`formulas/<id>.py`, 2000)

Formulas are user-authored code that grows over time — the same failure shape as the hook `description` that motivated the original pre-flight. The server's error for them is positional and names no datapoint, so a local check is worth more here than anywhere else.

**Files:**
- Modify: `src/snapshot/limits.rs` (new constant)
- Modify: `src/cli/push/scan.rs` (`field_limit_violations`)
- Test: `src/cli/push/scan.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `check_text` (Task 3); `crate::snapshot::schema::read_local_formulas(queue_dir: &Path) -> Result<Vec<(String, Vec<u8>)>>` (already exists, returns `(datapoint_id, bytes)` sorted by id).
- Produces: `pub const SCHEMA_FORMULA_LIMIT: usize = 2000;`

The `ChangeList.schemas` map stores the path to `schema.json`; the formulas directory is `schema.json`'s parent joined with `formulas`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/cli/push/scan.rs`:

```rust
    /// Schema formulas live in `formulas/<id>.py` and are spliced back into
    /// the schema body on push. The server caps them at 2000 characters and
    /// answers an over-length one with a POSITIONAL error carrying no
    /// datapoint id at all, so naming the file is the whole point.
    #[test]
    fn field_limit_violations_reports_oversized_schema_formula() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path().join("workspaces/main/queues/invoices");
        std::fs::create_dir_all(queue_dir.join("formulas")).unwrap();
        std::fs::write(
            queue_dir.join("schema.json"),
            br#"{"name":"Invoices","content":[]}"#,
        )
        .unwrap();
        std::fs::write(
            queue_dir.join("formulas/total_amount.py"),
            "x".repeat(2001).as_bytes(),
        )
        .unwrap();
        std::fs::write(
            queue_dir.join("formulas/vendor_name.py"),
            "y".repeat(2000).as_bytes(),
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.schemas
            .insert("invoices".to_string(), queue_dir.join("schema.json"));

        let v = cl.field_limit_violations();
        assert_eq!(v.len(), 1, "only the over-length formula should flag: {v:?}");
        assert_eq!(v[0].kind, "schemas");
        assert_eq!(v[0].field, "formula on datapoint 'total_amount'");
        assert_eq!(v[0].limit, 2000);
        assert_eq!(v[0].actual, 2001);
        assert_eq!(v[0].path, queue_dir.join("formulas/total_amount.py"));
    }

    /// A queue with no `formulas/` directory must not error.
    #[test]
    fn schema_without_formulas_dir_is_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path().join("workspaces/main/queues/invoices");
        std::fs::create_dir_all(&queue_dir).unwrap();
        std::fs::write(
            queue_dir.join("schema.json"),
            br#"{"name":"Invoices","content":[]}"#,
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.schemas
            .insert("invoices".to_string(), queue_dir.join("schema.json"));

        assert_eq!(cl.field_limit_violations().len(), 0);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cli::push::scan::tests::field_limit_violations_reports_oversized_schema_formula`
Expected: FAIL — `only the over-length formula should flag: []`.

- [ ] **Step 3: Implement**

In `src/snapshot/limits.rs`, add next to `RULE_TRIGGER_CONDITION_LIMIT`:

```rust
/// A schema datapoint's `formula` is capped at 2000 characters. Like
/// `trigger_condition` it is extracted to a sidecar (`formulas/<id>.py`)
/// and so cannot live in [`field_limits`]. The server's rejection for this
/// field is positional and carries no datapoint id, which is why naming the
/// sidecar locally is worth more here than for a top-level field.
pub const SCHEMA_FORMULA_LIMIT: usize = 2000;
```

In `src/cli/push/scan.rs`, after the rules-sidecar loop from Task 4, add:

```rust
        // Schema formulas live in `<queue_dir>/formulas/<id>.py` and are
        // spliced back into the body on push. `ChangeList.schemas` stores the
        // path to `schema.json`, so the queue dir is its parent.
        for (slug, schema_path) in &self.schemas {
            let Some(queue_dir) = schema_path.parent() else {
                continue;
            };
            let formulas =
                crate::snapshot::schema::read_local_formulas(queue_dir).unwrap_or_default();
            for (id, bytes) in formulas {
                let Ok(text) = String::from_utf8(bytes) else {
                    continue; // not UTF-8 — the push path surfaces that
                };
                if let Some(v) = crate::snapshot::limits::check_text(
                    format!("formula on datapoint '{id}'"),
                    crate::snapshot::limits::SCHEMA_FORMULA_LIMIT,
                    &text,
                ) {
                    out.push(FieldLimitViolation {
                        kind: "schemas",
                        slug: slug.clone(),
                        path: queue_dir.join("formulas").join(format!("{id}.py")),
                        field: v.field,
                        limit: v.limit,
                        actual: v.actual,
                    });
                }
            }
        }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib cli::push::scan`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot/limits.rs src/cli/push/scan.rs
git commit -m "feat(limits): validate schema formulas against the 2000-char cap

Formulas are user-authored code that grows over time -- the same failure
shape as the hook description that motivated the pre-flight. They were
unchecked because they live in formulas/<id>.py sidecars.

Worth more here than for a top-level field: the server's rejection is
positional and names no datapoint at all, so the local check supplies the
one piece of information the 400 omits.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Validate nested schema fields (`prompt` 5000, `memory.index_formula` 2000, inline `formula`)

These stay inside `schema.json`, nested in the content tree. The walk must mirror the recursion `extract_formulas` / `merge_formulas` already use: array `children` for sections and tuples, a **single object** `children` for a multivalue — otherwise line-item column fields are missed.

An **inline** `formula` is also checked here. `merge_formulas` splices a sidecar only when the datapoint has no `formula` key, so a formula written directly into `schema.json` is pushed as-is and must be validated too.

**Files:**
- Modify: `src/snapshot/limits.rs` (new constants + walker)
- Modify: `src/cli/push/scan.rs` (dispatch inside `field_limit_violations`)
- Test: `src/snapshot/limits.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `check_text` (Task 3), `SCHEMA_FORMULA_LIMIT` (Task 5).
- Produces:
  - `pub const SCHEMA_PROMPT_LIMIT: usize = 5000;`
  - `pub const SCHEMA_INDEX_FORMULA_LIMIT: usize = 2000;`
  - `pub fn check_schema_content(body: &Value) -> Vec<LimitViolation>`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/snapshot/limits.rs`:

```rust
    /// `prompt` and `memory.index_formula` stay inline in `schema.json`.
    #[test]
    fn schema_nested_prompt_and_index_formula_are_checked() {
        let body = json!({
            "content": [{
                "category": "section",
                "id": "invoice_details",
                "children": [{
                    "category": "datapoint",
                    "id": "invoice_id",
                    "prompt": "p".repeat(5001),
                    "memory": { "index_formula": "m".repeat(2001) }
                }]
            }]
        });
        let got = check_schema_content(&body);
        assert_eq!(
            got,
            vec![
                LimitViolation {
                    field: "prompt on datapoint 'invoice_id'".to_string(),
                    limit: 5000,
                    actual: 5001,
                },
                LimitViolation {
                    field: "memory.index_formula on datapoint 'invoice_id'".to_string(),
                    limit: 2000,
                    actual: 2001,
                },
            ],
        );
    }

    /// A multivalue's `children` is a single OBJECT, not an array. Missing
    /// that descent silently skips every line-item column — the most likely
    /// way to write this walk wrong.
    #[test]
    fn schema_walk_descends_into_line_item_columns() {
        let body = json!({
            "content": [{
                "category": "section",
                "id": "line_items_section",
                "children": [{
                    "category": "multivalue",
                    "id": "line_items",
                    "children": {
                        "category": "tuple",
                        "id": "line_item",
                        "children": [{
                            "category": "datapoint",
                            "id": "item_total",
                            "formula": "f".repeat(2001)
                        }]
                    }
                }]
            }]
        });
        assert_eq!(
            check_schema_content(&body),
            vec![LimitViolation {
                field: "formula on datapoint 'item_total'".to_string(),
                limit: 2000,
                actual: 2001,
            }],
        );
    }

    /// Boundaries, and the trailing-newline case the server trims.
    #[test]
    fn schema_nested_values_at_limit_are_accepted() {
        let body = json!({
            "content": [{
                "category": "section",
                "id": "s",
                "children": [{
                    "category": "datapoint",
                    "id": "d",
                    "prompt": "p".repeat(5000),
                    "formula": format!("{}\n", "f".repeat(2000)),
                    "memory": { "index_formula": "m".repeat(2000) }
                }]
            }]
        });
        assert_eq!(check_schema_content(&body), vec![]);
    }

    /// A schema with no content, or datapoints carrying none of these keys,
    /// must produce nothing and never panic.
    #[test]
    fn schema_walk_tolerates_missing_and_non_string_values() {
        assert_eq!(check_schema_content(&json!({})), vec![]);
        assert_eq!(check_schema_content(&json!({ "content": [] })), vec![]);
        let body = json!({
            "content": [{
                "category": "datapoint",
                "id": "d",
                "prompt": 42,
                "memory": "not-an-object"
            }]
        });
        assert_eq!(check_schema_content(&body), vec![]);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib snapshot::limits`
Expected: FAIL to compile — `check_schema_content` does not exist yet.

- [ ] **Step 3: Implement the walker**

In `src/snapshot/limits.rs`:

```rust
/// A schema datapoint's `prompt` is capped at 5000 characters, and
/// `memory.index_formula` at 2000. Both stay inline in `schema.json`
/// (unlike `formula`, which is extracted to a sidecar).
pub const SCHEMA_PROMPT_LIMIT: usize = 5000;
pub const SCHEMA_INDEX_FORMULA_LIMIT: usize = 2000;

/// Check the length-capped fields nested inside a schema's content tree.
///
/// Walks the same shape `snapshot::schema::extract_formulas` walks:
/// `children` is an array for sections and tuples but a single object for
/// a multivalue (its element schema). Descending into both is what covers
/// line-item column fields rather than only top-level datapoints.
///
/// `formula` is checked here too even though it normally lives in a
/// sidecar: `merge_formulas` splices a sidecar only when the datapoint has
/// no `formula` key, so one written directly into `schema.json` is pushed
/// verbatim and must be validated.
pub fn check_schema_content(body: &Value) -> Vec<LimitViolation> {
    let mut out = Vec::new();
    if let Some(content) = body.get("content").and_then(|c| c.as_array()) {
        for node in content {
            walk_schema_node(node, &mut out);
        }
    }
    out
}

fn walk_schema_node(node: &Value, out: &mut Vec<LimitViolation>) {
    let Some(obj) = node.as_object() else { return };

    if obj.get("category").and_then(|c| c.as_str()) == Some("datapoint") {
        let id = obj.get("id").and_then(|i| i.as_str()).unwrap_or("<unnamed>");

        if let Some(Value::String(s)) = obj.get("prompt") {
            out.extend(check_text(
                format!("prompt on datapoint '{id}'"),
                SCHEMA_PROMPT_LIMIT,
                s,
            ));
        }
        if let Some(Value::String(s)) = obj.get("formula") {
            out.extend(check_text(
                format!("formula on datapoint '{id}'"),
                SCHEMA_FORMULA_LIMIT,
                s,
            ));
        }
        if let Some(Value::String(s)) = obj.get("memory").and_then(|m| m.get("index_formula")) {
            out.extend(check_text(
                format!("memory.index_formula on datapoint '{id}'"),
                SCHEMA_INDEX_FORMULA_LIMIT,
                s,
            ));
        }
    }

    match obj.get("children") {
        Some(Value::Array(children)) => {
            for child in children {
                walk_schema_node(child, out);
            }
        }
        Some(child @ Value::Object(_)) => walk_schema_node(child, out),
        _ => {}
    }
}
```

Note the assertion order in the first test: `prompt`, then `memory.index_formula`. The implementation emits `prompt`, `formula`, `memory.index_formula` in that order, and the test's datapoint has no inline `formula`, so the two orders agree.

- [ ] **Step 4: Wire it into the scan**

In `src/cli/push/scan.rs`, inside the `check` closure in `field_limit_violations`, replace the single `for v in ...check_field_limits(kind, &body)` loop with:

```rust
                let nested = match kind {
                    "schemas" => crate::snapshot::limits::check_schema_content(&body),
                    _ => Vec::new(),
                };
                for v in crate::snapshot::limits::check_field_limits(kind, &body)
                    .into_iter()
                    .chain(nested)
                {
                    out.push(FieldLimitViolation {
                        kind,
                        slug: slug.clone(),
                        path: path.clone(),
                        field: v.field,
                        limit: v.limit,
                        actual: v.actual,
                    });
                }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib snapshot::limits && cargo test --lib cli::push::scan`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/snapshot/limits.rs src/cli/push/scan.rs
git commit -m "feat(limits): validate nested schema prompt and index_formula

Both stay inline in schema.json. The walk mirrors extract_formulas: array
children for sections and tuples, a single object child for a multivalue --
without that descent every line-item column field is silently skipped.

An inline formula is checked here too, since merge_formulas only splices a
sidecar when the datapoint has no formula key, so one written straight into
schema.json is pushed verbatim.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Validate `rules actions[].payload.content` (4096)

The `OPTIONS` metadata nests these under a polymorphic wrapper (`actions.child.show_message.payload.content`), but real rules serialize each action **flat** with a `type` discriminator. A walker written from the metadata would match nothing — this shape was confirmed against live rules.

**Files:**
- Modify: `src/snapshot/limits.rs` (constant + walker)
- Modify: `src/cli/push/scan.rs` (extend the `match kind` dispatch from Task 6)
- Modify: `src/snapshot/limits.rs` (`no_validated_field_is_stripped_before_push` invariant test)
- Test: `src/snapshot/limits.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `check_text` (Task 3).
- Produces:
  - `pub const RULE_ACTION_CONTENT_LIMIT: usize = 4096;`
  - `pub fn check_rule_actions(body: &Value) -> Vec<LimitViolation>`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/snapshot/limits.rs`. The fixture uses the **verified wire shape**, not the `OPTIONS` wrapper:

```rust
    /// Real rules serialize each action FLAT with a `type` discriminator —
    /// `{"id","enabled","type","event","payload"}` — not under the
    /// polymorphic wrapper the OPTIONS metadata implies. A walker written
    /// from the metadata alone would match nothing.
    #[test]
    fn rule_action_payload_content_is_checked() {
        let body = json!({
            "name": "Example Rule",
            "actions": [
                {
                    "id": "b7d5856b-7990-4c8f-8048-ca3b8e68239a",
                    "enabled": true,
                    "type": "show_message",
                    "event": "validation",
                    "payload": { "type": "warning", "content": "ok", "schema_id": "total" }
                },
                {
                    "id": "cf3e8c84-552c-482c-b1cf-333ace397a8c",
                    "enabled": true,
                    "type": "add_automation_blocker",
                    "event": "validation",
                    "payload": { "content": "c".repeat(4097), "schema_id": "total" }
                }
            ]
        });
        assert_eq!(
            check_rule_actions(&body),
            vec![LimitViolation {
                field: "actions[1] (add_automation_blocker) payload.content".to_string(),
                limit: 4096,
                actual: 4097,
            }],
        );
    }

    #[test]
    fn rule_action_content_at_limit_is_accepted() {
        let body = json!({
            "actions": [{
                "type": "show_message",
                "payload": { "content": "c".repeat(4096) }
            }]
        });
        assert_eq!(check_rule_actions(&body), vec![]);
    }

    /// Rules with no actions, actions with no payload, and non-string
    /// content must all be tolerated without panicking.
    #[test]
    fn rule_actions_walk_tolerates_missing_and_malformed() {
        assert_eq!(check_rule_actions(&json!({})), vec![]);
        assert_eq!(check_rule_actions(&json!({ "actions": [] })), vec![]);
        assert_eq!(
            check_rule_actions(&json!({ "actions": [{ "type": "custom" }] })),
            vec![]
        );
        assert_eq!(
            check_rule_actions(&json!({ "actions": [{ "payload": { "content": 7 } }] })),
            vec![]
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib snapshot::limits`
Expected: FAIL to compile — `check_rule_actions` does not exist.

- [ ] **Step 3: Implement**

In `src/snapshot/limits.rs`:

```rust
/// A rule action's `payload.content` — the message text shown to the
/// operator — is capped at 4096 characters.
pub const RULE_ACTION_CONTENT_LIMIT: usize = 4096;

/// Check the length-capped fields inside a rule's `actions` array.
///
/// The `OPTIONS` metadata nests these under a polymorphic wrapper
/// (`actions.child.show_message.payload.content`), but that is a metadata
/// artifact — the same one hooks exhibit. On the wire each action is flat
/// with a `type` discriminator, so one uniform path covers every action
/// kind. Only `payload.content` is validated: the sibling `id` is a
/// server-generated UUID and `payload.schema_id` is bounded by the schema
/// field id rules, so neither is free text a human can overgrow.
pub fn check_rule_actions(body: &Value) -> Vec<LimitViolation> {
    let mut out = Vec::new();
    let Some(actions) = body.get("actions").and_then(|a| a.as_array()) else {
        return out;
    };
    for (i, action) in actions.iter().enumerate() {
        let Some(Value::String(content)) = action.get("payload").and_then(|p| p.get("content"))
        else {
            continue;
        };
        let ty = action
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("action");
        out.extend(check_text(
            format!("actions[{i}] ({ty}) payload.content"),
            RULE_ACTION_CONTENT_LIMIT,
            content,
        ));
    }
    out
}
```

In `src/cli/push/scan.rs`, extend the dispatch added in Task 6:

```rust
                let nested = match kind {
                    "schemas" => crate::snapshot::limits::check_schema_content(&body),
                    "rules" => crate::snapshot::limits::check_rule_actions(&body),
                    _ => Vec::new(),
                };
```

- [ ] **Step 4: Extend the strip invariant to the new locations**

The existing `no_validated_field_is_stripped_before_push` test guards top-level fields only. A nested check has the same hazard: if `strip_for_create` ever removed `content` or `actions`, the walk would flag a value the server never sees. Add to `mod tests` in `src/snapshot/limits.rs`:

```rust
    /// Same invariant as `no_validated_field_is_stripped_before_push`, for
    /// the containers the nested walks descend into. If either were ever
    /// stripped before push, the walk would reject a value that never
    /// reaches the wire — a false positive strictly worse than the 400.
    #[test]
    fn nested_walk_containers_are_not_stripped_before_push() {
        for (kind, container) in [("schemas", "content"), ("rules", "actions")] {
            let mut body = json!({ container: [] });
            crate::snapshot::create::strip_for_create(&mut body, kind);
            assert!(
                body.get(container).is_some(),
                "{kind}.{container} is walked for nested limits but stripped before push",
            );
        }
    }
```

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: all green (lib ~896+ and integration ~219+, plus the tests added by this plan).

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/snapshot/limits.rs src/cli/push/scan.rs
git commit -m "feat(limits): validate rule action payload.content against 4096

Confirmed the wire shape against live rules first: each action serializes
flat with a type discriminator, NOT under the polymorphic wrapper the
OPTIONS metadata implies -- a walker written from the metadata alone would
have matched nothing.

Only payload.content is validated. The sibling id is a server-generated
UUID and payload.schema_id is bounded by schema field id rules, so neither
is free text a human can overgrow; validating them would be pure
false-positive surface.

Also extends the strip invariant to the containers the nested walks descend
into, so a future strip rule cannot silently turn a check into a false
positive.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Verification before calling this done

- [ ] `cargo test` fully green.
- [ ] `cargo clippy --all-targets -- -D warnings` clean.
- [ ] Do **not** run `cargo fmt` repo-wide; a pre-existing fmt-check failure is expected and is not a regression.
- [ ] Install the build and confirm against a real project: `cargo install --path . --force`, then `rdc doctor <env> --dry-run` on a project with a long schema formula. Remember `doctor` **writes** unless `--dry-run` is passed.
- [ ] Confirm the pre-flight is genuinely early: with an over-length field present, `rdc sync <env>` should fail in about a second, and should fail the same way even with a deliberately invalid token in `secrets/<env>.secrets.json` — proving no network call was needed.
- [ ] Leave all commits on local `main`. Do not push.
