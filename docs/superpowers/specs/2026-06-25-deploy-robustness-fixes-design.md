# Deploy-robustness fixes (live-surfaced) — design

Date: 2026-06-25
Status: approved (brainstorming), pending implementation plan

## 1. Overview

Running the new live integration suite against a real Rossum test sandbox
surfaced three issues with `rdc`'s cross-environment deploy
(`migrate` → `sync`). This spec covers fixes for the two real `rdc` gaps and
records the verified resolution of the third:

- **① `run_after` deferred relink for hooks** — `sync` cannot deploy a hook
  whose `run_after` points at a not-yet-created hook (create-time hook→hook
  ref). Real `rdc` bug; clean fix reusing the existing two-phase relink.
- **② Adopt existing email_templates on push** — deploying a queue to a fresh
  env fails (or silently duplicates) on the server's auto-created default
  email_templates. Real `rdc` gap; fix by matching + adopting existing remote
  templates instead of blindly POSTing.
- **③ Soft-deleted (async) queues** — queue DELETE is async
  (`deletion_requested`, ~24h purge). **Verified: no `rdc` change needed** —
  rdc already treats soft-deleted queues as deleted. Small live-test-harness
  alignment + documentation only.

The two fixes are independent (different push drivers) but share one
acceptance gate: once both land, the `live_deploy_flow` test drops the two
workarounds it currently uses (stripping `run_after` hooks and email_templates
from the prod snapshot before push) and asserts the full graph deploys.

## 2. Verified background facts

All grounded in the code and live probes against the test sandbox (not assumed):

- The two-phase **deferred relink** infra is kind-generic and already wired:
  `crate::snapshot::refs::resolve_value_deferring(&mut Value, &Lockfile) ->
  Vec<(String, Value)>` strips top-level fields that still hold unresolvable
  `rdc://` refs and returns them; push drivers collect them as
  `crate::cli::push::relink::DeferredRelink { kind, slug, path, fields }`; the
  orchestrator runs `relink::run_relink(...)` after all drivers, which
  re-resolves + PATCHes them once every object exists
  (`src/cli/sync/execute.rs:3339,3354`).
  - **Queues** use it: `src/cli/push/queues.rs:44` (defer) + `:71-77` (collect).
  - **Engines** use it: `src/cli/push/engines.rs` (same pattern).
  - **Hooks do NOT**: `src/cli/push/hooks.rs` calls the non-deferring
    `resolve_value` at `:112` (create), `:250` and `:290` (patch paths), and
    `hooks::push` has **no `relink` parameter** (`src/cli/push/hooks.rs:49`).
  - The orchestrator (`src/cli/push/mod.rs:43`) already holds a `relink:
    &mut Vec<DeferredRelink>` and passes it to queues (`:55`) and engines
    (`:83`), but not to hooks (`:71`).
- The fail-loud guard `ensure_no_residual_refs` (`src/api/mod.rs`) rejects any
  POST/PATCH body still containing an `rdc://` ref — this is what makes an
  un-deferred hook `run_after` fail with "refusing to send /hooks: body still
  contains unresolved portable reference(s)".
- **email_templates** (live): a queue auto-creates **5 default templates**.
  Their `type` values: `rejection_default` and
  `email_with_no_processable_attachments` are **unique** (a second POST with
  the same type → `400 "Cannot create template with unique type: …"`); the
  three "Annotation status change" defaults have `type: "custom"`, and a
  second `type:"custom"` POST **succeeds** (201) — i.e. it silently
  duplicates. A default template **can be PATCHed** (200). The pulled template
  JSON carries a stable `type` field (in the model's flattened `extra`).
  - `src/cli/push/email_templates.rs`: the **no-lockfile-entry branch POSTs
    blindly** (`:42-75`); the lockfile-entry branch PATCHes (`:77-211`) and
    already caches a remote listing (`:103-110`).
- **Soft-deleted queues** (live): `DELETE /queues/{id}` → `202` with
  `status:"deletion_requested"`; the queue lingers ~24h, and **its
  `workspace` becomes `null`**. `PATCH /queues/{id} {schema:null}` → `400
  "This field may not be null."` (so detach-before-delete is impossible). A
  queue's schema stays `409`-referenced until the queue purges.
  - **rdc already excludes workspace-null queues from pull**
    (`src/cli/sync/mod.rs:568`, regression test `:1396` for the exact
    `deletion_requested`/`workspace:null` case) — so soft-deleted queues are
    already "considered deleted" by rdc and never re-pulled.

## 3. ① `run_after` deferred relink for hooks

### Cause
Hooks never feed the deferred-relink mechanism, so a hook body whose
`run_after` references a hook not yet created in this push reaches the API with
an unresolved `rdc://hooks/<slug>` and is rejected by `ensure_no_residual_refs`.

### Fix
Make hooks participate in the existing relink machinery, exactly as queues do:

1. Add `relink: &mut Vec<crate::cli::push::relink::DeferredRelink>` to
   `hooks::push` (`src/cli/push/hooks.rs:49`).
2. Thread it at the call site `src/cli/push/mod.rs:71`.
3. On the **create path** (`hooks.rs:112`): replace
   `resolve_value(&mut payload, lockfile)` with
   `let deferred = resolve_value_deferring(&mut payload, lockfile);`. After the
   successful POST + lockfile `upsert`, if `!deferred.is_empty()`, push a
   `DeferredRelink { kind: "hooks".into(), slug, path: <hook json path>, fields:
   deferred }` onto `relink`.
4. Leave the **patch paths** (`:250`, `:290`) on `resolve_value`, mirroring
   queues (an update to an already-existing hook resolves its `run_after`
   normally; only create-time forward/cyclic refs need deferral).

The existing `run_relink` pass needs **no change**: it PATCHes `/{kind}/{id}`
(verified `src/cli/push/relink.rs`), which for hooks resolves to `/hooks/{id}`
(endpoint == kind), and does post-write bookkeeping via `codec("hooks")` (which
exists). Hooks only need to start producing `DeferredRelink` entries.

### Scope note
This covers any top-level hook field holding an unresolvable `rdc://hooks/…`
ref, not just `run_after` — `resolve_value_deferring` is field-agnostic.

### Backward compatibility
Additive parameter; hooks with no deferred refs are byte-for-byte unchanged.
Hooks that previously failed the push now succeed. No on-disk/lockfile format
change. Idempotency is preserved by the existing post-push portabilize pass.

### Tests
- Unit/`wiremock` in `tests/` (mirror the queue deferred-relink test): a hook
  with `run_after` → another hook created in the same push; assert the create
  POST body has `run_after` stripped, and a follow-up PATCH (relink) sets
  `run_after` to the resolved URL. Assert the push succeeds.
- Live: `live_deploy_flow` stops stripping `run_after` hooks (see §6).

## 4. ② Adopt existing email_templates on push

### Cause
On a cross-env deploy the target queue's lockfile has no entry for the migrated
default templates, so rdc takes the blind-POST branch. For unique types that
`400`s; for `type:"custom"` defaults it silently creates duplicates.

### Fix
In the no-lockfile-entry branch of `src/cli/push/email_templates.rs`, before
POSTing, look up the target **queue's** existing remote templates and try to
**match** the local one:

- **Match key:** the template's `type` when `type != "custom"`; otherwise the
  template's `name`. (Unique-type defaults match by `type`; the custom
  defaults — and any custom template — match by `name`.) Matching is scoped to
  the same queue.
- **On match:** adopt the matched remote `id` into the lockfile and **PATCH**
  it with the local content (defaults are PATCH-able), instead of POSTing.
- **On no match:** POST as today (a genuinely new template).

Reuse the remote-template listing the PATCH branch already fetches
(`email_templates.rs:103-110`) so we list once. Wrap per-item push failures in
skip-and-continue (mirroring the DELETE driver's error isolation,
`src/cli/push/deletes.rs`) so a residual `400` never aborts the whole batch.

### Backward compatibility
Only the no-lockfile-entry path changes. Same-env sync (entry exists → PATCH)
is untouched. No snapshot/codec/pull change. Conservative trade-off (consistent
with how DELETE already skips): a user-authored custom template whose `name`
collides with an existing remote template on the same queue is **adopted**
(PATCHed) rather than duplicated — documented; the user can rename to force a
new one.

### Tests
- `wiremock` in `tests/`: (a) POST-path with no lockfile entry + a remote
  template matching by `type` → assert adopt-id + PATCH (no POST); (b) match by
  `name` for a `type:"custom"` template; (c) no match → POST; (d) a stray
  `400` on POST is skipped, not fatal.
- Live: `live_deploy_flow` stops stripping email_templates (see §6).

## 5. ③ Soft-deleted queues — no rdc change

**Resolution (verified):** soft-deleted queues have `workspace:null` and rdc's
pull already skips workspace-null queues, so rdc already treats them as
deleted. `detach-via-PATCH` is impossible (`schema:null` → 400). No rdc code
change.

**Harness alignment (small):** the live-test teardown/janitor currently list
*all* prefix-matched queues, so they re-DELETE already-soft-deleted queues
(→ `400`) and report "N settling". Update the harness queue listing
(`tests/live/support/client.rs` `list_ids_by_name_prefix` for queues, and the
janitor) to **exclude `deletion_requested`/workspace-null queues** — treating
them as deleted, per the project rule "soft-deleted queues shall be considered
deleted". This removes the noise and is purely test-side.

**Documentation:** note in the README "Live integration testing" section (and a
comment near the delete driver) that queue DELETE is async (`deletion_requested`,
~24h purge), schemas referenced by a soft-deleting queue orbit until purge, and
this is expected (not an rdc defect).

## 6. Live acceptance (shared gate)

Once ① and ② land, edit `tests/live/scenarios/deploy_flow.rs` to **remove both
workarounds**: the `run_after`-hook strip (+ its queue-ref scrub) and the
default-email_template strip. The scenario should then deploy the full migrated
graph and pass. Re-run the whole live suite
(`cargo test --test live -- --ignored --test-threads=1`) to confirm 7/7 green.

## 7. Backward compatibility (summary)

- No on-disk snapshot or lockfile-format changes.
- ① and ② only change code paths that today *fail* (run_after deploy) or
  *error/silently-duplicate* (cross-env default templates); same-env sync is
  unchanged.
- ③ is test-side + docs only.
- The default (`cargo test`) suite stays green; live scenarios remain
  `#[ignore]`.

## 8. Risks / open items

- `run_relink` PATCHing the `hooks` kind: **confirmed** — `/{kind}/{id}` →
  `/hooks/{id}`, `codec("hooks")` exists. No change needed.
- `type` field present on email_templates: **confirmed live** on both unique
  defaults and `type:"custom"` templates.
- The ② `name` fallback's collision trade-off is intentional; revisit only if a
  real customer hits it.
- Minor: the `run_relink` api-path comment says "endpoint == kind for
  queues/engines" — true for hooks too; update the comment when touching it.
