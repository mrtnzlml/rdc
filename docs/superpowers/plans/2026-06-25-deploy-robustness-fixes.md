# Deploy-robustness Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rdc` deploy the full graph to a fresh environment: hooks with `run_after` cross-refs (① deferred relink) and queues whose server-default email_templates already exist on the target (② adopt-on-push), plus align the live harness with "soft-deleted queues are deleted" (③).

**Architecture:** ① and ② are surgical changes to two existing push drivers (`src/cli/push/hooks.rs`, `src/cli/push/email_templates.rs`) that reuse machinery already present for other kinds (the two-phase deferred-relink for queues/engines; the drift-adopt pattern). ③ is test-harness-only (filter soft-deleted queues) plus docs. A final task drops the two workarounds the live deploy-flow scenario currently uses and re-runs the live suite as the end-to-end gate.

**Tech Stack:** Rust 2024, `tokio`, `wiremock` (mocked integration tests in `tests/`), `assert_cmd`, the `rdc` push pipeline (`src/cli/push/*`), the deferred-relink module (`src/cli/push/relink.rs`), `serde_json`.

## Global Constraints

- No new dependencies; no on-disk snapshot or lockfile-format changes.
- Output PRISTINE — zero warnings; the default `cargo test` stays green; all live scenarios remain `#[ignore]`.
- No customer identifiers or sandbox coordinates (host/org id/token) anywhere; neutral placeholders only.
- `resolve_value_deferring(value: &mut serde_json::Value, lockfile: &Lockfile) -> Vec<(String, serde_json::Value)>` resolves what it can and returns the deferred top-level fields (original pre-resolution values).
- `DeferredRelink { kind: String, slug: String, path: std::path::PathBuf, fields: Vec<(String, serde_json::Value)> }`. The existing `run_relink` PATCHes `/{kind}/{id}` (works for `hooks` → `/hooks/{id}`); **no relink-phase change needed**.
- ① is scoped to the hook **create path** only (sufficient for the deploy-flow gate, which deploys all hooks to a fresh env). The patch path is intentionally out of scope (documented in code).
- ② matches by `type` when `type != "custom"`, else by `name`, within the same `queue`; on match adopt the remote id + PATCH; else POST; wrap POST failures in skip-and-continue.
- ③ "soft-deleted queues shall be considered deleted": filter queues with `status == "deletion_requested"` or null `workspace` out of the harness's queue listings.

---

## Task 1: ① Hook `run_after` deferred relink

**Files:**
- Modify: `src/cli/push/hooks.rs` (signature ~49-58; create path ~110-112 and after the create upsert ~215-224)
- Modify: `src/cli/push/mod.rs` (hooks call ~71)
- Test: `tests/cli_sync.rs` (new wiremock test, mirroring the existing queue/engine deferred-relink test)

**Interfaces:**
- Consumes: `crate::snapshot::refs::resolve_value_deferring(&mut Value, &Lockfile) -> Vec<(String, Value)>`; `crate::cli::push::relink::DeferredRelink`; the orchestrator's `relink: &mut Vec<relink::DeferredRelink>` (already in `push_classified`, `src/cli/push/mod.rs:43`).
- Produces: `hooks::push(paths, client, lockfile, interactive, changes, catalog_hooks, relink, progress, env)` — new `relink` param inserted **after `catalog_hooks`** (matching `queues::push`'s position).

- [ ] **Step 1: Write the failing wiremock test**

First read the existing deferred-relink integration test to mirror its scaffolding:
Run: `grep -rn "DeferredRelink\|run_relink\|deferred relink\|run_after" tests/ | head` and read the matching test (the 2026-06-17 deferred-relink work added one for queues/engines). Mirror its MockServer setup.

Add to `tests/cli_sync.rs` a test `sync_push_hook_run_after_deferred_relink` that, against a `wiremock` MockServer, bootstraps a project (via the existing `init` + secrets helper used by other tests in this file) with **two new local hooks** in `envs/dev/hooks/`:
- `validator.json`: `{"name":"Validator","type":"function","events":["annotation_content"],"queues":[],"config":{"runtime":"python3.12"}}` + `validator.py` sidecar `def f(p):\n    return {}\n`
- `post-validator.json`: same shape, plus `"run_after": ["rdc://hooks/validator"]` + `post-validator.py` sidecar.
Neither hook is in the lockfile (both are creates). Mock:
- `POST /api/v1/hooks` → return a created hook with a fresh id (stateful: first call id=901 for whichever is created first, second id=902). Capture request bodies.
- `PATCH /api/v1/hooks/901` and `/902` → echo the body with the id.
- the usual empty list endpoints (use the file's `mock_empty_lists_except` helper) + organization GET.
Drive the push (the test should call the same entry the other push tests use — `rdc::cli::sync::run("dev", false, false, false, false, true)` i.e. `--no-pull` deploy mode, or the push path other tests use). Assert:
- The `post-validator` **create POST body does NOT contain `run_after`** (it was deferred/stripped).
- A **PATCH** to the post-validator hook id was sent whose body has `run_after` resolved to the validator hook's **URL** (`{base}/hooks/901`), not `rdc://`.
- The push returns success.

```rust
// Skeleton — fill mock bodies per the file's existing patterns:
#[tokio::test]
async fn sync_push_hook_run_after_deferred_relink() {
    let _g = cwd_lock();
    let server = MockServer::start().await;
    // ... init project at a tempdir, write validator.json/.py + post-validator.json/.py,
    //     write secrets, point rdc.toml at server.uri() ...
    // ... mount stateful POST /hooks, PATCH /hooks/{id}, empty lists, org GET ...
    // run push (no-pull deploy)
    let result = /* rdc::cli::sync::run(...) or the push entry used by sibling tests */;
    assert!(result.is_ok());
    // inspect server.received_requests(): find POST /hooks for post-validator,
    // assert its JSON body has no "run_after"; find the PATCH that sets run_after
    // to ".../hooks/901".
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test cli_sync sync_push_hook_run_after_deferred_relink -- --nocapture`
Expected: FAIL — today the create POST body still contains the unresolved `rdc://hooks/validator` (and `ensure_no_residual_refs` makes the create error), so the assertions/`is_ok()` fail.

- [ ] **Step 3: Add the `relink` parameter to `hooks::push`**

In `src/cli/push/hooks.rs`, change the signature (currently lines 49-58) to insert `relink` after `catalog_hooks`:
```rust
pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    interactive: bool,
    changes: &BTreeMap<String, std::path::PathBuf>,
    catalog_hooks: &[crate::model::Hook],
    relink: &mut Vec<crate::cli::push::relink::DeferredRelink>,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
```

- [ ] **Step 4: Thread `relink` at the call site**

In `src/cli/push/mod.rs` line 71, change:
```rust
    hooks::push(paths, client, lockfile, interactive, &changes.hooks, catalog_hooks, progress, env)
```
to:
```rust
    hooks::push(paths, client, lockfile, interactive, &changes.hooks, catalog_hooks, relink, progress, env)
```

- [ ] **Step 5: Defer on the hook create path**

In `src/cli/push/hooks.rs`, the create branch currently does (around line 110-112):
```rust
            let mut payload = read_hook_value(&hooks_dir, slug)
                .with_context(|| format!("reading local hook '{slug}' for create"))?;
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);
```
Change the resolve line to capture deferred fields:
```rust
            let mut payload = read_hook_value(&hooks_dir, slug)
                .with_context(|| format!("reading local hook '{slug}' for create"))?;
            // Two-phase relink: resolve what we can; defer top-level fields whose
            // rdc:// refs target a hook not yet created (e.g. `run_after` pointing
            // at another new hook). The relink pass PATCHes them once all hooks
            // exist. (Patch path intentionally not deferred — see plan ① scope.)
            let deferred = crate::snapshot::refs::resolve_value_deferring(&mut payload, lockfile);
```
Then, immediately **after** the post-create `lockfile.upsert("hooks", slug, …)` block (around lines 215-224), add:
```rust
            if !deferred.is_empty() {
                relink.push(crate::cli::push::relink::DeferredRelink {
                    kind: "hooks".to_string(),
                    slug: slug.clone(),
                    path: local_json_path.clone(),
                    fields: deferred,
                });
            }
```
(`slug` and `local_json_path` are the loop variables from `for (slug, local_json_path) in changes` at line 98.)

> Note: the create path has a store-extension (install) sub-branch and a regular-hook sub-branch; `resolve_value_deferring` runs once before the branch (shared), and the `relink.push` runs once after the shared `upsert`, so both sub-branches are covered. If `deferred` must be referenced after a sub-branch `continue`, hoist the `relink.push` to run before any `continue` in the create branch — verify there is no early `continue` between the `upsert` and the new block; if there is, place the `relink.push` immediately before it.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test --test cli_sync sync_push_hook_run_after_deferred_relink -- --nocapture`
Expected: PASS — create POST omits `run_after`; the relink PATCH sets it to the resolved URL.

- [ ] **Step 7: Run the broader push tests + clippy**

Run: `cargo test --test cli_sync` then `cargo clippy --all-targets 2>&1 | grep -E "warning|error" | head`
Expected: all cli_sync tests pass; no new warnings/errors from the changed files.

- [ ] **Step 8: Commit**

```bash
git add src/cli/push/hooks.rs src/cli/push/mod.rs tests/cli_sync.rs
git commit -m "fix(push): defer hook run_after cross-refs via the existing relink pass"
```

---

## Task 2: ② Adopt existing email_templates on push

**Files:**
- Modify: `src/cli/push/email_templates.rs` (no-lockfile-entry POST branch ~29-75; reuse the remote-list pattern from ~103-110)
- Test: `tests/cli_sync.rs` (new wiremock test, mirroring the existing email_template push test there)

**Interfaces:**
- Consumes: `client.list_email_templates(Some(progress.clone())) -> Result<Vec<EmailTemplate>>`; `EmailTemplate { id, url, name, subject, queue: Option<String>, extra }` with `type` in `extra` (read via `extra.get("type").and_then(|v| v.as_str())`); `client.update_email_template(id, &EmailTemplate, progress)`; `lockfile.upsert("email_templates", key, ObjectEntry{..})`.
- Produces: no signature change; only the no-entry branch's behavior changes (adopt-or-POST).

- [ ] **Step 1: Write the failing wiremock test**

Read the existing email_template push test in `tests/cli_sync.rs` (around lines 8511-8760 — the one with the stateful `GET /api/v1/email_templates` counter mock and `POST /api/v1/email_templates`) and mirror its scaffolding.

Add `sync_push_email_template_adopts_existing_by_type`: a fresh project (empty `email_templates` lockfile) with one local template file whose body has `"type":"rejection_default"`, `"queue":"rdc://queues/<q-slug>"` (with the queue already lockfile-pinned to a known id so the ref resolves to `{base}/queues/<id>`), `"name":"Default rejection template"`, `"subject":"local subject"`. Mock:
- `GET /api/v1/email_templates` → returns ONE existing remote template `{id: 555, queue: "{base}/queues/<id>", type: "rejection_default", name: "...", subject: "remote subject"}`.
- `PATCH /api/v1/email_templates/555` → echo body with id 555.
- `POST /api/v1/email_templates` → mount with `.expect(0)` (must NOT be called).
Drive the push. Assert:
- the push succeeds,
- **no POST** was sent to `/email_templates` (adopt path taken),
- a **PATCH /email_templates/555** was sent,
- the lockfile now has the template keyed with `id: 555`.

Add a second test `sync_push_email_template_posts_when_no_match`: same setup but the remote list returns a template with a DIFFERENT `type` (e.g. `email_with_no_processable_attachments`), so no match → assert a `POST` is sent (`.expect(1)`).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test cli_sync sync_push_email_template_adopts_existing_by_type sync_push_email_template_posts_when_no_match -- --nocapture`
Expected: the adopt test FAILS — today the no-entry branch POSTs unconditionally (POST is hit; `.expect(0)` fails / no PATCH). The no-match test may pass already (POST happens) but keep it as the regression guard for the new branch.

- [ ] **Step 3: Implement adopt-or-POST in the no-entry branch**

In `src/cli/push/email_templates.rs`, replace the body of the no-lockfile-entry branch (currently the block at lines 29-75 that reads the payload, resolves refs, strips, and POSTs) so that, after building the resolved+typed local template, it first tries to adopt an existing remote template:

```rust
            let disk_bytes = std::fs::read(template_path)
                .with_context(|| format!("reading {}", template_path.display()))?;
            let mut payload: serde_json::Value = serde_json::from_slice(&disk_bytes)
                .with_context(|| format!("parsing {}", template_path.display()))?;
            crate::snapshot::refs::resolve_value(&mut payload, lockfile);

            // Identify the local template's queue + match key BEFORE stripping.
            let local: crate::model::EmailTemplate = serde_json::from_value(payload.clone())
                .with_context(|| format!("deserializing local email template '{lockfile_key}'"))?;
            let local_type = local.extra.get("type").and_then(|v| v.as_str());
            let local_queue = local.queue.clone();

            // Populate the remote cache once (same pattern as the PATCH branch).
            if remote_cache.is_empty() {
                for r in client.list_email_templates(Some(progress.clone())).await
                    .context("listing email templates to adopt server-managed defaults")?
                {
                    remote_cache.insert(r.id, r);
                }
            }

            // Match an existing remote template on the SAME queue by `type`
            // (unless "custom") else by `name`. Rossum auto-creates default
            // templates per queue (some unique-typed -> POST 400s; the custom
            // ones -> silent duplicates), so adopt rather than POST.
            let adopt = remote_cache.values().find(|r| {
                r.queue == local_queue
                    && match local_type {
                        Some(t) if t != "custom" => {
                            r.extra.get("type").and_then(|v| v.as_str()) == Some(t)
                        }
                        _ => r.name == local.name,
                    }
            }).cloned();

            if let Some(remote) = adopt {
                // Adopt the remote id into the lockfile, then PATCH local content.
                let id = remote.id;
                strip_patch_extra(&mut local.extra.clone(), "email_templates", false);
                let mut to_send = local.clone();
                strip_patch_extra(&mut to_send.extra, "email_templates", false);
                let updated = client.update_email_template(id, &to_send, Some(progress.clone()))
                    .await
                    .with_context(|| format!("PATCH /email_templates/{id} (adopting existing)"))?;
                let codec = crate::snapshot::codec::codec("email_templates").unwrap();
                let updated_art = codec.disk_bytes(&serde_json::to_value(&updated)
                    .context("serializing adopted email template")?)
                    .context("codec disk_bytes for adopted email template")?;
                let updated_hash = combined_hash(&updated_art.json, &updated_art.sidecars, lockfile);
                crate::state::base_cache::write_disk_and_cache(paths, template_path, &updated_art.json)
                    .with_context(|| format!("writing adopted form for '{lockfile_key}'"))?;
                lockfile.upsert("email_templates", lockfile_key, ObjectEntry {
                    id,
                    modified_at: updated.modified_at().map(|s| s.to_string()),
                    content_hash: Some(updated_hash),
                    secrets_hash: None,
                });
                progress.event(Action::Patch, &format!("email_template/{lockfile_key} adopted existing id={id}"));
                pushed += 1;
                continue;
            }

            // No existing match → POST as before (skip-and-continue on failure).
            strip_for_create(&mut payload, "email_templates");
            match client.create_email_template(&payload, Some(progress.clone())).await {
                Ok(created) => {
                    let codec = crate::snapshot::codec::codec("email_templates").unwrap();
                    let created_art = codec.disk_bytes(&serde_json::to_value(&created)
                        .context("serializing created email template")?)
                        .context("codec disk_bytes for created email template")?;
                    let created_hash = combined_hash(&created_art.json, &created_art.sidecars, lockfile);
                    write_atomic(template_path, &created_art.json)
                        .with_context(|| format!("writing post-create form for '{lockfile_key}'"))?;
                    lockfile.upsert("email_templates", lockfile_key, ObjectEntry {
                        id: created.id,
                        modified_at: created.modified_at().map(|s| s.to_string()),
                        content_hash: Some(created_hash),
                        secrets_hash: None,
                    });
                    progress.event(Action::Post, &format!("email_template/{lockfile_key} id={}", created.id));
                    pushed += 1;
                }
                Err(e) => {
                    // Skip-and-continue (mirror the DELETE driver): a stray 400
                    // (e.g. unique-type) must not abort the whole push.
                    progress.event(Action::Warn, &format!("email_template/{lockfile_key} create failed (skipped): {e:#}"));
                    skipped += 1;
                }
            }
            continue;
```

> Implementer notes:
> - `remote_cache` is the same `HashMap<u64, EmailTemplate>` the PATCH branch lazily fills (declared above the loop). If it is currently declared inside/after the no-entry branch, move its declaration above the loop so both branches share it.
> - `strip_for_create`, `strip_patch_extra`, `combined_hash`, `write_atomic`, `Action`, `ObjectEntry` are already imported in this file (used by the existing branches). Remove the accidental duplicate `strip_patch_extra(&mut local.extra.clone(), …)` line above — only the `to_send` strip is needed (it's shown twice as a transcription artifact; keep one).
> - `local` must be `mut` if you strip its extra; or build `to_send` from `local.clone()` and strip that. Keep exactly one strip on the value actually sent.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test cli_sync sync_push_email_template_adopts_existing_by_type sync_push_email_template_posts_when_no_match -- --nocapture`
Expected: PASS — adopt test sends PATCH (no POST); no-match test sends POST.

- [ ] **Step 5: Run broader push tests + clippy**

Run: `cargo test --test cli_sync` then `cargo clippy --all-targets 2>&1 | grep -E "warning|error" | head`
Expected: green; no new warnings.

- [ ] **Step 6: Commit**

```bash
git add src/cli/push/email_templates.rs tests/cli_sync.rs
git commit -m "fix(push): adopt existing email_templates by type/name instead of blind POST"
```

---

## Task 3: ③ Harness treats soft-deleted queues as deleted + docs

**Files:**
- Modify: `tests/live/support/client.rs` (`list_ids_by_name_prefix` queue case; `schema_ids_for_queue_prefix`)
- Modify: `tests/live/scenarios/janitor.rs` (assert queues empty after the filter)
- Modify: `README.md` ("Live integration testing" section — add the async-delete note)

**Interfaces:**
- Consumes: `Queue { name, workspace: Option<String>, extra }` (status lives in `extra`); the existing `list_*`/`to_values` helpers.
- Produces: `list_ids_by_name_prefix("queue", …)` and `schema_ids_for_queue_prefix(…)` now exclude soft-deleted queues (`status == "deletion_requested"` or null `workspace`).

- [ ] **Step 1: Filter soft-deleted queues in `list_ids_by_name_prefix`**

In `tests/live/support/client.rs`, in `list_ids_by_name_prefix`, replace the result-collection loop so a soft-deleted queue is skipped for the `queue` kind:
```rust
        let mut out = Vec::new();
        for v in values {
            // Treat soft-deleted queues as deleted: Rossum's async queue DELETE
            // returns 202 `deletion_requested` and nulls the workspace; such
            // queues linger ~24h but are gone for our purposes.
            if kind == "queue" {
                let soft_deleted = v.get("status").and_then(|s| s.as_str()) == Some("deletion_requested")
                    || v.get("workspace").map(|w| w.is_null()).unwrap_or(true);
                if soft_deleted {
                    continue;
                }
            }
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.starts_with(prefix) {
                if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                    out.push((id, name.to_string()));
                }
            }
        }
        Ok(out)
```

- [ ] **Step 2: Filter soft-deleted queues in `schema_ids_for_queue_prefix`**

In the same file, in `schema_ids_for_queue_prefix`, skip soft-deleted queues:
```rust
        let queues = self.inner.list_queues(None).await?;
        let mut out = Vec::new();
        for q in queues {
            let soft_deleted = q.workspace.is_none()
                || q.extra.get("status").and_then(|s| s.as_str()) == Some("deletion_requested");
            if soft_deleted {
                continue;
            }
            if q.name.starts_with(prefix) {
                if let Some(url) = q.schema.as_deref() {
                    if let Some(id) = url.trim_end_matches('/').rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) {
                        out.push(id);
                    }
                }
            }
        }
        Ok(out)
```

- [ ] **Step 3: Tighten the janitor (soft-deleted queues now filtered → assert clean)**

In `tests/live/scenarios/janitor.rs`, replace the trailing "queues still settling" report block with a hard assertion (soft-deleted queues are now excluded from the listing, so any remaining queue is a real leak):
```rust
    // Soft-deleted queues (status `deletion_requested` / workspace null) are
    // treated as deleted and excluded from the listing, so the sweep must leave
    // no live queues behind either.
    let queues_left = client.list_ids_by_name_prefix("queue", RunId::marker()).await.unwrap_or_default();
    assert!(queues_left.is_empty(), "janitor left live queue objects: {queues_left:?}");
}
```

- [ ] **Step 4: Add the README note**

In `README.md`, in the "Live integration testing" section, after the "sweep leftovers" code block (the `cargo test --test live live_janitor_sweep …` block, ~line 264), insert:
```markdown
> Queue deletion is asynchronous on Rossum: `DELETE` returns `202`
> (`deletion_requested`) and the queue lingers ~24h before purge, with its
> `workspace` nulled. The harness (and `rdc`'s pull) treat such soft-deleted
> queues as deleted, so they are never re-pulled or counted. A queue's schema
> stays referenced (and so undeletable) until the queue actually purges — that
> transient schema orphan is expected, not an `rdc` defect.
```

- [ ] **Step 5: Compile + hermetic check**

Run: `cargo test --test live --no-run` then `env -u RDC_LIVE_API_BASE -u RDC_LIVE_ORG_ID -u RDC_LIVE_TOKEN cargo test --test live 2>&1 | tail -3`
Expected: compiles warning-free; the live binary's hermetic unit tests pass and all live scenarios report `ignored`. (The queue-filter + janitor change is live-verified in Task 4.)

- [ ] **Step 6: Commit**

```bash
git add tests/live/support/client.rs tests/live/scenarios/janitor.rs README.md
git commit -m "test(live): treat soft-deleted queues as deleted + document async delete"
```

---

## Task 4: Drop deploy-flow workarounds + full live regression (credentialed)

**Files:**
- Modify: `tests/live/scenarios/deploy_flow.rs` (remove the `run_after`-hook strip + queue-ref scrub, and the default-email_templates strip)

**Interfaces:** Consumes the now-fixed ① and ②.

> **Credential boundary:** this task's verification runs the live suite against a real sandbox (`RDC_LIVE_*`). The code edit + compile are doable anywhere; the live run is the maintainer's.

- [ ] **Step 1: Remove the email-templates strip**

In `tests/live/scenarios/deploy_flow.rs`, delete the block that walks `envs/prod/workspaces/*/queues/*/email-templates` and `remove_dir_all`s them (added as a workaround). ② now adopts the target's auto-created defaults on push.

- [ ] **Step 2: Remove the run_after-hook strip + queue-ref scrub**

In the same file, delete the block that collects `run_after` hook json files, deletes them (+ `.py`), and strips their refs from each `queue.json` `hooks` array. ① now deploys `run_after` hooks via deferred relink.

- [ ] **Step 3: Compile + skip-verify**

Run: `cargo test --test live --no-run` then `cargo test --test live live_deploy_flow -- --ignored` (no creds)
Expected: compiles; prints the skip message and exits 0.

- [ ] **Step 4: Live regression (maintainer, with creds)**

Run:
```bash
export RDC_LIVE_API_BASE=... RDC_LIVE_ORG_ID=... RDC_LIVE_TOKEN=...
cargo test --test live live_deploy_flow -- --ignored --nocapture
```
Expected: PASS — the full migrated graph (incl. `run_after` hooks and default email_templates) deploys; teardown clean.

- [ ] **Step 5: Full live suite (maintainer)**

Run: `cargo test --test live -- --ignored --test-threads=1`
Expected: 7 passed, 0 failed. Then a janitor sweep to confirm no live leftovers: `cargo test --test live live_janitor_sweep -- --ignored`.

- [ ] **Step 6: Commit**

```bash
git add tests/live/scenarios/deploy_flow.rs
git commit -m "test(live): deploy-flow deploys the full graph (run_after hooks + default templates)"
```

---

## Self-Review (completed during planning)

**1. Spec coverage:**
- ① run_after deferred relink (spec §3) → Task 1. ✓
- ② email_template adopt-on-push (spec §4) → Task 2. ✓
- ③ soft-deleted queues — harness filter + docs (spec §5) → Task 3. ✓
- Shared live acceptance gate (spec §6) → Task 4. ✓
- Backward compat (spec §7): additive `relink` param; only-currently-failing paths change; test-side ③ — preserved across tasks. ✓

**2. Placeholder scan:** The wiremock tests in Tasks 1–2 give concrete bodies, mocks, and assertions but instruct mirroring the *named existing* tests for boilerplate — this is "follow existing codebase patterns," not a cross-task placeholder. The ② code block flags one transcription artifact (duplicate `strip_patch_extra`) to remove. No "TBD"/"implement later".

**3. Type consistency:** `relink: &mut Vec<crate::cli::push::relink::DeferredRelink>` matches `queues::push`/`engines::push` and the orchestrator's `relink`. `resolve_value_deferring -> Vec<(String, Value)>`, `DeferredRelink{kind,slug,path,fields}`, `EmailTemplate{queue, extra}`, `lockfile.upsert("…", key, ObjectEntry{id, modified_at, content_hash, secrets_hash})`, `client.{list_email_templates,update_email_template,create_email_template}` — all match the verbatim signatures extracted from the code.
