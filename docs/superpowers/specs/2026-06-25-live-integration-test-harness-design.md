# Live integration test harness — design

Date: 2026-06-25
Status: approved (brainstorming), pending implementation plan

## 1. Overview

A **repeatable, expandable, opt-in** test suite that drives the real `rdc`
binary against a real Rossum test organization and verifies *both* sides of
every operation — the remote API state **and** the resulting local files and
lockfile.

Today every test in `tests/*.rs` mocks the API with `wiremock`; the only live
verification is a manual procedure written up in
`docs/superpowers/notes/2026-06-07-live-verification-stage1.md`. This harness
turns that manual procedure into a structured, re-runnable Rust test suite
without disturbing the existing mocked suite or CI.

The canonical scenario is a **round-trip**:

```
seed remote (out-of-band, direct API)
  → rdc sync <test>            (PULL)   → assert local snapshot + lockfile
  → mutate local files         (edit)
  → rdc sync <test>            (PUSH)   → assert remote API state
  → rdc migrate <test> <prod>  (local)  → assert migrated local snapshot
  → rdc sync <prod>            (PUSH)   → assert remote (deploy flow)
  → teardown (delete everything created, correct order)
```

## 2. Goals / Non-goals

### Goals
- Prove the full `deploy = migrate + sync` pipeline against the real API.
- Cover four edge-case families (see §6): collisions & identity, cross-refs &
  portable refs, sidecars & redaction, conflicts & deletes.
- Be **repeatable**: each run fully cleans up after itself; leftover state from
  a crashed run never breaks the next run.
- Be **expandable**: adding a new edge case is dropping files into a static
  folder plus (at most) a small scenario function — not rewriting plumbing.
- Be **backward compatible**: `cargo test` is completely unaffected; no
  production code behavior changes.

### Non-goals
- Running in CI. CI (`release.yaml`) does not run tests today and this suite
  stays manual/opt-in. (A future CI lane is possible but out of scope.)
- Replacing the wiremock suite. The mocked tests remain the fast inner loop;
  this harness is the slow, high-fidelity outer loop.
- Exhaustively exercising kinds the org/plan forbids creating (engines,
  workflows). Where the API refuses, the harness skips-with-log rather than
  failing (see §10).

## 3. Background facts (verified, grounded in the repo)

These are confirmed from the code, not assumed:

- **Crate shape.** `rdc` builds both a binary (`src/main.rs`) and a library
  (`src/lib.rs`, crate name `rdc`). `lib.rs` re-exports `pub mod api`, `config`,
  `secrets`, `paths`, `state`, `snapshot`, `slug`, … so the harness can use the
  real client and helpers directly.
- **API client is already fully public.** `rdc::api::RossumClient` with
  `new(base_url: String, token: String) -> Result<Self>`; `base_url` is the env
  `api_base` *verbatim* (`src/cli/sync/mod.rs:228`:
  `RossumClient::new(env_cfg.api_base.clone(), token)`). It exposes
  `create_*(&serde_json::Value)`, `get_*(id)`, `list_*()`, `update_*(id, ..)`,
  `delete_*(id)`, plus generic `delete_path(path)` and `patch_value(path, body)`.
  The client carries the 10 req/s limiter, pagination, and retry/backoff. **No
  production-code change is needed to use it from tests.**
- **Progress handle.** `ProgressHandle = Option<Arc<crate::log::Log>>`
  (`src/api/retry.rs:31`). A silent handle is simply `None`.
- **Project config.** `rdc.toml` → `[envs.<name>]` with `api_base: String` and
  `org_id: u64` (`src/config/mod.rs`).
- **Secrets.** `secrets/<env>.secrets.json` = `{api_token, expires_at?,
  username?, password?}` (`src/secrets.rs`). Token resolution also honors
  `RDC_TOKEN_<ENV>` / `RDC_USER_<ENV>` / `RDC_PASS_<ENV>`.
- **Test ergonomics.** Existing tests spawn the binary with
  `assert_cmd::Command::cargo_bin("rdc")` and use `tempfile::TempDir`. The
  global `cwd_lock()` exists only because some tests `set_current_dir`;
  `assert_cmd` supports `.current_dir(tmp)`, so the live harness avoids
  mutating process-global cwd entirely.
- **`migrate` is pure-local, zero network** (`src/cli/migrate/mod.rs`); `sync`
  is the only remote-touching command. The documented flow is `migrate` then
  `sync` (`src/cli/mod.rs:146-180`).
- **On-disk layout** (per-env): `envs/<env>/{organization.json, labels/,
  hooks/, rules/, workspaces/<ws>/queues/<q>/{queue.json, schema.json,
  formulas/<field_id>.py, inbox.json, email-templates/<tpl>.json},
  engines/, workflows/, mdh/}`; state under `.rdc/state/<env>.lock.json` and
  `.rdc/state/<env>.base/`. Cross-refs on disk are portable
  `rdc://<kind>/<slug>`.
- **Real-org constraints observed historically** (to re-verify, not assume):
  engines `403` (plan-limited), schemas `409` when referenced, queue delete
  async (`202 deletion_requested`, ~24h purge), default email_templates `400`
  on direct delete. Per the maintainer, **all objects are creatable/deletable
  given correct call ordering** (e.g. delete the queue to drop its auto-created
  schema/templates rather than deleting those directly).

## 4. Architecture / components

All new code lives under `tests/` (and `testdata/live/`); the only crate
artifacts consumed are the public `rdc` lib and the `rdc` binary.

```
tests/
  live.rs                     # #[ignore] scenario tests (thin; compose support/)
  live/
    support/
      mod.rs
      config.rs               # LiveConfig: resolve RDC_LIVE_* env, skip if absent
      run_id.rs               # unique run id (time-nanos + pid), name prefixing
      client.rs               # thin wrappers over rdc::api::RossumClient (None progress)
      seeder.rs               # read manifest, resolve @kind/key refs, POST in order
      project.rs              # ProjectFixture: tempdir + rdc.toml + secrets + run rdc
      assert_local.rs         # load snapshot/lockfile, normalize, compare to expected
      assert_remote.rs        # GET remote, assert structure/resolved refs
      teardown.rs             # RAII Drop guard: delete created objects, correct order
      janitor.rs              # sweep stale rdc-it-* objects (safety net)
testdata/live/
  manifest.toml
  bodies/<kind>/<key>.json
  bodies/<kind>/<key>.{py,js}
  expected/<scenario>.toml
```

### Component responsibilities

- **`LiveConfig`** — reads `RDC_LIVE_API_BASE`, `RDC_LIVE_ORG_ID`,
  `RDC_LIVE_TOKEN` (optional `RDC_LIVE_USER`/`RDC_LIVE_PASS`). If any required
  value is missing, returns a sentinel that makes each scenario **skip with a
  printed reason** (early `return`, not a panic/failure). No org id or host is
  ever hardcoded in the repo.
- **`run_id`** — one unique token per process run, derived from
  `SystemTime` nanos + `std::process::id()` (base36). No new dependency. Every
  seeded object's *name* is `rdc-it-<run_id>-<logical-name>`; slugs derive from
  names, so the namespace flows to disk and the lockfile for free.
- **`client`** — constructs `RossumClient::new(api_base, token)` and offers
  typed helpers (`create(kind, body)`, `get(kind, id)`, `list(kind)`,
  `delete(kind, id)`) that pass `None` as the progress handle.
- **`Seeder`** — interprets the static folder (§5): topologically orders objects
  by declared deps, substitutes `@kind/key` placeholders with the real URL of
  the already-created dependency, POSTs each body, and records a `SeedIndex`
  (logical key → `{id, url, name, slug-as-server-stores-it}`).
- **`ProjectFixture`** — creates a `TempDir`, writes `rdc.toml` (envs `test`
  and `prod`, both pointing at the live org) and `secrets/<env>.secrets.json`,
  and runs the real binary via `Command::cargo_bin("rdc").current_dir(tmp)`.
  Captures stdout/stderr/exit for assertions. No `set_current_dir`.
- **`assert_local`** — reads files from the tempdir, strips/normalizes volatile
  fields (ids, timestamps, server emails), and compares against `expected/*`
  declarations or property assertions.
- **`assert_remote`** — GETs objects by id and asserts key fields and that
  cross-refs resolved to real URLs (not `rdc://`).
- **`Teardown`** — an RAII guard owning the `SeedIndex` plus any ids created via
  push; on `Drop` (including panic) it deletes in dependency order
  (engine_fields → engines → labels → rules → hooks → email_templates →
  inboxes → queues → schemas → workspaces), idempotently, tolerating
  already-gone / async-delete responses.
- **`janitor`** — a separate `#[ignore]` test that lists the org and deletes any
  `rdc-it-*` object whose embedded timestamp is older than a threshold, for the
  case where a hard crash skipped a `Drop`.

## 5. The static folder (`testdata/live/`)

Declarative so new edge cases need no Rust. Bodies are raw Rossum API create
payloads (the same shape as `testdata/fixtures/`), keeping the harness in
control of *exactly* what hits the remote — including same-name objects that
`rdc`'s local authoring would dedup but the API accepts.

### `manifest.toml`
```toml
# Ordered list; deps drive topological create order and ref substitution.
[[object]]
key   = "ws-alpha"            # logical key, unique within the folder
kind  = "workspace"
body  = "bodies/workspaces/ws-alpha.json"
tags  = ["core"]

[[object]]
key   = "queue-invoices-alpha"
kind  = "queue"
body  = "bodies/queues/invoices-alpha.json"
deps  = ["ws-alpha", "schema-invoices-alpha"]
tags  = ["core", "collision"]

[[object]]
key   = "queue-invoices-beta"  # SAME display name "Invoices", different workspace
kind  = "queue"
body  = "bodies/queues/invoices-beta.json"
deps  = ["ws-beta", "schema-invoices-beta"]
tags  = ["collision"]
```

### Bodies and placeholders
A body is a normal JSON create payload. Cross-references to other seeded
objects use a `@kind/key` placeholder that the seeder resolves to the real URL
*after* the dependency is created:
```json
{
  "name": "Invoices",
  "workspace": "@workspace/ws-alpha",
  "schema": "@schema/schema-invoices-alpha",
  "hooks": ["@hook/validator"]
}
```
Sidecar code is a sibling file referenced from the body (e.g. a hook body names
`bodies/hooks/validator.py`); the seeder inlines it into `config.code` before
POSTing, mirroring how `rdc` extracts it on pull.

The seeder injects the `rdc-it-<run_id>-` name prefix at POST time, so the
static files themselves stay stable and diff-clean across runs.

### `expected/<scenario>.toml`
Post-pull expectations, expressed against normalized local state. The block
below is **illustrative of the format only** — the concrete values are NOT
asserted as fact here (see "Authoring expectations" below):
```toml
# slugs the pull must produce
[[slug]]
kind = "queues"
present = ["<ws-a>/invoices", "<ws-b>/invoices"]     # exact form captured from a reviewed run

[[ref]]
file  = "envs/test/workspaces/<ws-a>/queues/invoices/queue.json"
field = "schema"
value = "rdc://schemas/<...>"                        # exact form captured from a reviewed run

[[lockfile]]
kind = "queues"
keys = ["<...>", "<...>"]
```

### Authoring expectations (no-assumptions policy)
The recon left the *exact* on-disk form of cross-workspace same-name objects
genuinely ambiguous (composite `<ws>/<q>` keys vs. global `-2` dedup vs.
id-pinned global slugs — the codebase has carried more than one of these). The
collisions scenario exists precisely to **pin** that behavior, so the spec must
not pre-decide it. Therefore:

- **Known behavior** → write the expected values directly in `expected/*.toml`.
- **Uncertain behavior** (e.g. cross-workspace dedup, exact composite ref form)
  → run once, **capture** the actual normalized local state into the
  `expected/*.toml`, have the maintainer **review it for correctness**, then
  commit it as the golden expectation. Subsequent runs assert against it.

This capture-then-review step keeps the test grounded in observed behavior while
the human gate prevents pinning a bug as "expected".

## 6. Scenarios (one `#[ignore]` test each)

Each is `#[ignore = "live: needs RDC_LIVE_* env"]` and composes the support
primitives. All own a `Teardown` guard.

1. **`live_round_trip_core`** — seed the full graph → `sync test` (pull) →
   assert local (layout, slugs, rdc:// refs, lockfile, sidecars, redaction) →
   mutate one object of each kind → `sync test` (push) → assert remote reflects
   the edits → teardown.
2. **`live_collisions_identity`** — seed same-name queues/schemas/inboxes across
   two workspaces (and same-name labels/hooks where the API allows) → pull →
   assert slug dedup and global per-kind lockfile keys; then rename an object on
   the remote → re-pull → assert the on-disk slug is stable (id-pinned).
3. **`live_cross_refs`** — seed queue↔inbox, queue→schema, queue→hooks
   (`run_after`), rule→queue, and a **cyclic** hook `run_after` chain → pull →
   assert rdc:// rewrite; push a fresh copy → assert two-phase deferred relink
   resolves all refs on the remote.
4. **`live_sidecars_redaction`** — seed hooks with `.py` and `.js` code, a rule
   `trigger_condition`, schema formulas → pull → assert sidecar extraction and
   combined-hash parity; assert `status`/`counts`/inbox `email` redaction and
   secret sentinels never corrupt the round-trip.
5. **`live_conflicts_deletes`** — construct both-diverged and
   local-edit-vs-remote-delete states → run `sync` non-interactively and assert
   the deterministic outcomes (shadow file on content conflict; abort on
   remote-delete conflict; `--allow-deletes` for local-tombstone → remote
   DELETE; remote delete → local mirror). See §7 for the interactive variant.
6. **`live_deploy_flow`** — pull `test` → `rdc migrate test prod` → `rdc sync
   prod` → assert the remote now has the prod objects with refs resolved, and
   the local `prod` snapshot + lockfile are correct.

   **Same-org disambiguation (important):** `test` and `prod` point at the *same*
   org, and `migrate` auto-matches same-slug pairs (identity) by default — so an
   identity migrate would give `prod` objects names identical to `test`'s,
   colliding in the shared org and breaking teardown-by-prefix. The scenario
   therefore writes an explicit `.rdc/map/test-to-prod.toml` that renames every
   object for `prod` (e.g. a `-prod` slug suffix). Result: `prod` objects are
   distinct from `test` objects, still carry the `rdc-it-<run_id>-` prefix
   (inherited from the pulled `test` names) for cleanup, and the assertion can
   confirm both the rename and the ref resolution. This also exercises the
   non-identity ref-substitution path in `migrate` (`build_subst`).

## 7. Conflict-testing mechanism (decision)

Under `assert_cmd`, the child's stdin is piped → non-TTY → `sync` auto-enables
`--yes` and conflict prompts fall through to the **legacy shadow-file
behavior**; remote-delete conflicts **abort**. (Confirmed by the sync recon and
consistent with the maintainer's `script -q` PTY note.)

- **v1 (chosen):** assert these deterministic non-interactive outcomes. No new
  dependency, fully reproducible.
- **Future extension:** add a PTY driver (e.g. `expectrl` / `portable-pty`) to
  drive real `[k]`/`[r]`/`[K]`/`[R]` prompts and assert interactive resolution
  (including apply-to-all stickiness). This is additive, gated to the live
  suite, and introduces a dev-dependency only when implemented.

## 8. Assertion strategy

- **Structural, id/timestamp-normalized.** Never assert exact server ids,
  timestamps, or server-assigned emails. Strip them, then compare; or assert
  invariants ("exactly two queues share base name `Invoices`; on disk they live
  under distinct `<ws>/invoices` composite slugs").
- **Local:** read JSON + sidecars + lockfile from the tempdir; compare to
  `expected/*` or property assertions; `pretty_assertions` for diffs.
- **Remote:** GET by id; assert key fields and that cross-refs are real URLs
  (resolution happened), not `rdc://`.
- **Exit/output:** assert process exit code and (where stable) key log lines.

## 9. Lifecycle & teardown

- **Isolation:** per-run name prefix `rdc-it-<run_id>-` on every seeded object;
  workspace-scoped objects live under one run-id workspace, org-scoped objects
  (labels/hooks/rules) carry the prefix in their name. In the deploy-flow
  scenario, `prod` objects inherit the same run-id prefix (via the pulled `test`
  names) plus a `-prod` rename from the migrate mapping, so all of a run's
  objects — `test` and `prod` — share one prefix and are cleaned up together.
- **Teardown:** RAII `Drop` guard deletes in the correct dependency order,
  idempotent and tolerant of async/already-gone responses. Correct ordering
  yields a clean teardown (delete the queue to drop its schema/templates).
- **Janitor:** a separate `#[ignore]` sweep removes stale `rdc-it-*` objects if a
  prior run hard-crashed before `Drop`.
- **Serial by default:** run with `--test-threads=1` (shared org + 10 req/s).
  Per-run namespacing keeps parallelism a safe future option.

## 10. Risks / to verify live (not assume)

- Engine create (`403`?) and workflow create support on this org/plan — on
  refusal, skip-with-log instead of failing; keep those kinds out of the
  required happy path.
- Queue delete async semantics — teardown treats `202 deletion_requested` as
  success; janitor handles eventual purge.
- The historical email_template `400`-on-direct-delete — confirm that deleting
  the parent queue removes its templates, and order teardown accordingly.
- Minor: lockfile `url`-stored vs `id`-derived discrepancy flagged during recon
  — irrelevant to migrate (network-free) and to these assertions, but note it.

## 11. Backward compatibility

- `#[ignore]` ⇒ `cargo test` runs nothing new; the suite is opt-in via
  `cargo test --test live -- --ignored --test-threads=1`.
- No production code is modified (the lib surface is already public). The only
  repo additions are `tests/live*` and `testdata/live/`.
- No new runtime dependencies; no new dev-dependencies for v1 (PTY dep deferred
  to the future interactive-conflict extension).
- Confidentiality: no org id, host, or customer identifier in the repo — all
  sandbox coordinates come from `RDC_LIVE_*` env vars; static-folder names use
  neutral placeholders (`acme`, `invoices`, `main`, …).

## 12. How to run (documented in README "Live integration testing")

```sh
export RDC_LIVE_API_BASE="https://<host>/v1"
export RDC_LIVE_ORG_ID="<org id>"
export RDC_LIVE_TOKEN="<token>"          # or RDC_LIVE_USER + RDC_LIVE_PASS
cargo test --test live -- --ignored --test-threads=1
# Cleanup safety net, if a run crashed:
cargo test --test live janitor -- --ignored
```

## 13. Future extensions (explicitly out of v1 scope)

- PTY-driven interactive conflict resolution (§7).
- Engines / workflows once creatable on the test plan.
- MDH dataset round-trip (data-storage seeding).
- A CI lane (manual-dispatch) that runs the suite with secrets.
- Watch-mode (`sync --watch`) live coverage.
