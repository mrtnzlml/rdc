# MDH safe-push fix + adversarial live coverage — design

Date: 2026-06-26
Status: approved (brainstorming), pending implementation plan

## 1. Overview

The live integration suite covers the core-API kinds but has **zero** MDH
(Master Data Hub) coverage, and adversarial probing of the existing MDH push
driver surfaced a real destructive bug. This spec covers two coupled
deliverables:

- **(A) Safe within-env MDH push drops.** `rdc sync`'s MDH push
  (`src/cli/push/mdh.rs::push_dataset`) computes a **2-way mirror** diff
  (`diff_indexes(.., mirror=true)`) of local vs. live-remote indexes. With
  `mirror=true` it **silently drops every remote-only index** — including
  indexes an admin added through the UI since the last sync. This directly
  contradicts the driver's own doc comment (`mdh.rs:67-69`: "an admin may
  have added indexes via the UI since the last sync and we don't want to
  silently drop those"). It is also un-gated: destructive index drops happen
  with no `--allow-deletes` opt-in, unlike every other deletable kind. Fix:
  make the within-env push **base-aware (3-way)** so admin-added remote-only
  indexes are never dropped, and gate genuine user-removal drops behind
  `--allow-deletes` (mirroring rdc's global delete gate).

- **(B) Adversarial, idempotent, isolated live coverage.** New `#[ignore]`
  live scenario(s) exercising the full MDH pull+push surface against the real
  sandbox, on a per-run throwaway collection that is never one of the real
  collections, with RAII teardown + janitor sweep so the suite stays
  repeatable.

The two are coupled: (B) is the integration gate that proves (A) on real
infrastructure (admin-added index survives; user-removed index is gated).

## 2. Verified background facts

All grounded in source and live probes against the test sandbox (org 214757),
not assumed:

- **Only one real caller of the diff/apply API.** `grep` shows the sole
  non-test caller of `push_dataset` is `src/cli/sync/execute.rs:3547`;
  `diff_indexes` / `apply_diff` are called only from within
  `src/cli/push/mdh.rs` (`diff_for_dataset` → `diff_indexes(.., true)`).
  `src/cli/deploy/selection.rs:18` states "MDH is not yet writable" — there is
  **no cross-env deploy path** for MDH. The `mirror=false` ("deploy without
  `--mirror`") branch of `diff_indexes` is therefore exercised only by its own
  unit tests today; the "deploy --mirror" wording in the `mdh.rs` doc comments
  is aspirational.
- **The bug.** `diff_indexes` with `mirror=true` (`mdh.rs:349-356`,
  `:375-382`) pushes every remote-only regular/search index name onto
  `plan.drop_*`. `apply_diff` then drops them. Because the diff has no notion
  of a *base* (last-synced) index set, it cannot tell "the user removed this
  index locally" (legit, should drop with opt-in) from "an admin added this
  index remotely after the last sync" (must survive). Both look like
  "remote-only" → both are dropped.
- **The base IS available.** MDH pull writes the base cache: sub-phase C
  (`src/cli/pull/mdh.rs:270-280`) calls `apply_pull_action(.., Some(ctx.paths))`,
  the base-cache-writing variant, for `<env>/mdh/<slug>/indexes.json`. So
  `crate::state::base_cache::read(paths, indexes_path)` returns the last-synced
  index set bytes (`Ok(None)` if never synced) — `base_cache::read(paths,
  env_file) -> Result<Option<Vec<u8>>>` (`src/state/base_cache.rs:88`). The
  path is under `env_root` (`dataset_dir` = `<root>/envs/<env>/mdh/<slug>/`,
  `src/paths.rs:220`), so `cache_mirror` resolves it.
- **Latent invariant violation.** `base_cache.rs:30-34` documents the
  invariant `content_hash(canonicalize(cache_bytes)) ==
  lockfile.objects[kind][slug].content_hash`. `push_dataset` refreshes the
  **lockfile** hash after a push (`mdh.rs:92-107`) but never writes the base
  cache, so post-push the base cache is stale relative to the lockfile — the
  invariant is already violated for MDH. The fix corrects this by writing the
  base cache alongside the lockfile-hash refresh.
- **Global delete-gate semantics to mirror** (`src/cli/push/deletes.rs:83-119`
  `confirm_or_refuse`): `allow_deletes` → proceed without prompt; else if
  `!interactive` → **bail** with "N object(s) marked for deletion but
  `--allow-deletes` was not passed…"; else (interactive) → `[y/N]` prompt,
  `Aborted` on decline. The object-delete phase wraps this in
  `progress.with_prompt(..)` (`execute.rs:3235`).
- **Call-site has what the fix needs.** The MDH dispatch function in
  `execute.rs` already has `no_push` (`:3077`), `allow_deletes` (`:3079`),
  `interactive` (in scope, used `:3238`), and `ctx.paths` in scope. The push
  is already gated `local_hash != lockfile_base` (`execute.rs:3534-3546`).
- **Data Storage API (live-verified on sandbox, throwaway collection).**
  Base derived by `derive_data_storage_base` (`src/config/mod.rs:41`): strip
  `api.` host prefix + `/v1` (or `/api/v1`) path suffix, append
  `/svc/data-storage/api` (e.g. `https://api.elis.rossum.ai/v1` →
  `https://elis.rossum.ai/svc/data-storage/api`). Auth is **`Bearer`**
  (`data_storage.rs:221`), not the core API's `token` scheme. Verified
  endpoints: `POST /v1/collections/create {collectionName}` → 200; `POST
  /v1/data/insert_one {collectionName, document}` (auto-creates collection);
  `POST /v1/indexes/create` → 202 **async**; `POST /v1/search_indexes/create`
  → 202 **async**; `POST /v1/collections/drop {collectionName}` → 202
  **async**. `indexes/list` / `search_indexes/list` return **names-only by
  default**, but rdc's client sends `{nameOnly:false}` (`data_storage.rs:67,75`)
  to get full definitions — so pull captures `{name,key,options…}`.
- **rdc's `DataStorageClient` is reusable from the test crate.**
  `DataStorageClient::new(base, token)` and `list_collections`, `list_indexes`,
  `list_search_indexes`, `create_index`, `create_search_index`, `drop_index`,
  `drop_search_index` are all `pub` (`data_storage.rs:53,60,66,74,86,110,131,155`).
  Missing from the client (needed only for test setup/teardown):
  `collections/create`, `data/insert_one`, `collections/drop`.

## 3. Deliverable A — safe within-env MDH push drops

### 3.1 New pure 3-way diff

Add a base-aware diff alongside the existing `diff_indexes`:

```
ThreeWayDiff {
    plan: DiffPlan,                      // creates + changed-def drop+recreate (ALWAYS applied)
    pending_regular_deletes: Vec<String>, // genuine user removals (GATED)
    pending_search_deletes: Vec<String>,  // genuine user removals (GATED)
}

diff_indexes_3way(
    base_regular, base_search,
    local_regular, local_search,
    remote_regular, remote_search,
) -> ThreeWayDiff
```

Per index name (regular and search computed independently; the implicit
`_id_` regular index is filtered from **all three** sides, extending the
existing `index_by_name(.., filter_id_index)` filter to the base too):

| in base | in local | in remote | classification | action |
|---|---|---|---|---|
| – | yes | – | local-only (user added) | `plan.create_*` |
| – | yes | yes, same def | already present | no-op |
| any | yes | yes, diff def | changed | `plan.drop_*` + `plan.create_*` (always; no in-place update verb) |
| yes | – | yes | **user removed** | `pending_*_deletes` (gated) |
| – | – | yes | **admin-added** | **survives — never dropped** |
| yes | – | – | removed both sides | no-op |

`defs_equivalent` (existing, `mdh.rs:408`) decides "same def" (strips
server-set `v`, canonicalizes key order).

**No-base fallback (`base == None`):** when there is no base (never synced, or
base cache absent), `pending_*_deletes` is **empty** — without a base we cannot
prove any remote-only index is a user removal, so we never drop one. Behavior
degrades to additive + changed-def-recreate, which is strictly safe (no silent
clobber). Creates and changed-def recreates still apply.

`push_dataset` calls `diff_indexes_3way` **directly**. The old within-env
helper `diff_for_dataset` (`mdh.rs:296`) and `diff_indexes(.., mirror)` are
**retained unchanged** (now test-only) as the documented additive/mirror
primitive and its regression tests for the not-yet-built MDH deploy writer — an
intentional keep, not dead-by-accident. (Their tests stay green; only
`push_dataset`'s call target moves to the 3-way diff.)

### 3.2 `push_dataset` changes

`push_dataset` gains three parameters: `paths: &crate::paths::Paths`,
`allow_deletes: bool`, `interactive: bool`. New flow:

1. Read + parse local `IndexSet` (unchanged).
2. Read base: `let base = base_cache::read(paths, indexes_path)?;` parse to
   `IndexSet` (empty `IndexSet::default()` when `None`).
3. Fetch live remote regular + search (unchanged).
4. `let diff = diff_indexes_3way(base.regular, base.search, local.regular,
   local.search, remote_regular, remote_search);`
5. **Gate** the pending deletes, mirroring `confirm_or_refuse`
   (`deletes.rs:83`). Let `pending = diff.pending_regular_deletes.len() +
   diff.pending_search_deletes.len()`. If `pending > 0`:
   - `allow_deletes` → fold `pending_*` into `plan.drop_*`; `skipped = false`.
   - else if `!interactive` → **bail** (`Err`) with a message paralleling the
     global gate: "N MDH index(es) on '<collection>' marked for deletion but
     `--allow-deletes` was not passed. Re-run with `--allow-deletes` …, or
     restore `indexes.json` to cancel."
   - else (interactive) → prompt once inside `progress.with_prompt(..)`
     ("Drop N remote MDH index(es) no longer present locally on
     '<collection>'? [y/N] ", listing the names; route via
     `stdin_coord::read_line_coordinated`). On `y` → fold into `plan.drop_*`,
     `skipped = false`; otherwise leave them out, `skipped = true`.
   When `pending == 0`, `skipped = false`.
6. `let ops = apply_diff(client, collection, slug, &plan, progress).await?;`
   (`apply_diff` unchanged — the drop-then-recreate same-name wait gates still
   apply to changed-def drops).
7. **Lockfile + base cache** refresh only when the push fully reconciled
   remote to local (`ops > 0 && !skipped`): refresh the `mdh_indexes` lockfile
   `content_hash` to `content_hash(local_raw)` (as today) **and** write the
   base cache: `base_cache::write(paths, indexes_path, &local_raw)?;`. This
   keeps base == local == lockfile (restoring the invariant) and makes the
   next sync's 3-way diff accurate. When `skipped == true`, neither is
   refreshed, so the next sync re-detects the divergence and re-gates the
   removal (idempotent, no silent forget). The current code's bug — refreshing
   the lockfile hash even when a removal was skipped, which would make the next
   sync's `local_hash == base` gate skip the dataset and silently forget the
   removal — is thereby avoided.

### 3.3 Wiring at the call site

`execute.rs:3547` passes the three new args: `&ctx.paths` (the dispatch holds
`ctx`), `allow_deletes`, `interactive`. No other call site exists.

### 3.4 Supporting changes

- `IndexSet` (`src/model/index_set.rs:7`): add `Default` to the derive list
  (additive; needed for the no-base empty set).
- Data Storage base derivation in the test crate: **no production change
  needed.** `rdc::config::EnvConfig` is already `pub` with `pub` fields and a
  `pub data_storage_base()` (`src/config/mod.rs:34`), so the test helper
  derives the same base via
  `rdc::config::EnvConfig { api_base, org_id }.data_storage_base()` — single
  source of truth, zero added surface.

### 3.5 Backward compatibility

- No on-disk snapshot or lockfile-format change. `indexes.json` shape,
  `mdh_indexes` lockfile kind, and codec output are unchanged.
- The only behavioral change is the within-env `rdc sync` MDH push:
  - Admin-added remote-only indexes now **survive** (previously silently
    dropped) — the bug fix.
  - Genuine user-removal drops now require `--allow-deletes` (previously
    un-gated) — consistent with every other deletable kind. A non-interactive
    sync that removed an index locally and previously dropped it silently will
    now bail until `--allow-deletes` is passed (intended, fail-loud).
  - Creates and changed-definition recreates are **unchanged** (always
    applied, never gated — a recreate is an update, not a delete).
- The latent base-cache invariant violation is fixed (now written on full
  push).
- `rdc deploy` is unaffected (MDH not deployable). `diff_indexes(.., mirror)`
  is retained with its tests.

### 3.6 Hermetic unit tests (run by default)

`diff_indexes_3way` is pure → cover every row of the §3.1 table for both
regular and search indexes:
- admin-added (remote-only, ∉base) → survives (no pending delete, no drop);
- user-removed (∈base, ∉local, ∈remote) → one pending delete, no create;
- local-only (∉remote) → create, no drop, no pending;
- changed-def (∈local ∩ ∈remote, diverging) → drop+create, **not** pending
  (ungated), independent of base membership;
- `v`-only difference → no-op;
- `_id_` filtered from base/local/remote → never pending/drop/create;
- no-base (`None`) → no pending deletes even with remote-only indexes; creates
  + changed-def still produced.

The gate decision (allow/interactive/bail) is small glue; its integration
behavior is proven by the live scenario (§4). Where cheaply factorable,
unit-test the gate's three-branch outcome on a synthetic pending list.

## 4. Deliverable B — adversarial, idempotent live coverage

### 4.1 Isolation & idempotency model

- **Throwaway collection per run:** `rdc_it_<run_id>_mdh`, where `<run_id>` is
  the harness's existing per-run id (sanitized to the Mongo-safe charset:
  hyphens → underscores). The scenario **never** references a real collection
  name; the real collection names never enter the repo (CLAUDE.md /
  no-customer-identifiers rule).
- **RAII teardown** drops the collection (`collections/drop`, async-202) at
  scenario end, on success or panic, mirroring the existing `Teardown`
  pattern (OS-thread drop, not `block_on` inside the test runtime — see the
  harness memory).
- **Janitor sweep** extends the existing pre-run janitor to list collections
  and drop any matching `rdc_it_*` (leftovers from a crashed prior run), so the
  suite is repeatable even after an abnormal exit.
- **Local project per run:** fresh temp dir → empty lockfile + base cache; the
  first `sync --no-push` seeds them. The whole scenario is re-runnable; an
  in-scenario "sync twice" step proves push idempotency (second run = 0 ops).

### 4.2 Harness additions

`tests/live/support/mdh.rs`:
- A thin raw client (reqwest, `Bearer`, base via
  `rdc::config::derive_data_storage_base(RDC_LIVE_API_BASE)`) for the **3**
  endpoints absent from rdc's client: `create_collection`, `insert_one`,
  `drop_collection`. Each handles the async 202 (poll
  `list_collections` / `list_indexes` to confirm convergence where needed).
- For everything else (seed regular/search indexes out-of-band; assert remote
  state), **reuse `rdc::api::DataStorageClient`** (`create_index`,
  `create_search_index`, `list_indexes`, `list_search_indexes`, `drop_index`,
  `drop_search_index`), passing `None` for the progress handle.
- Helpers to poll until an index appears/disappears (async create/drop).

Teardown/janitor (`tests/live/support/`): add MDH-collection drop + the
`rdc_it_*` sweep.

### 4.3 Scenario phases

A single ordered scenario on the run-id collection, each phase asserting
remote state (via `DataStorageClient` lists) **and** local state
(`indexes.json` + lockfile):

1. **Pull round-trip.** Out-of-band: create collection, insert a seed doc,
   create one regular index (`ix_a {a:1}`) and one search index. Poll until
   present. `rdc sync <env> --no-push`. Assert `indexes.json` =
   `{regular:[ix_a without _id_/v], search:[normalized {name,mappings,
   analyzers?}]}`; lockfile has an `mdh_indexes` entry for the slug.
2. **Push create.** Add a second regular index locally to `indexes.json`
   (`ix_b {b:-1}`). `rdc sync <env>`. Poll remote until `ix_b` present; assert
   present with the right key. Re-`sync` → 0 MDH ops (no spurious modify).
3. **Push modify.** Change `ix_a`'s key/options locally. `rdc sync <env>`.
   Assert the same-name drop+recreate completed (poll via the wait gate) and
   remote reflects the new def. Re-`sync` → clean.
4. **Safe-delete (gated — the fix).** Remove `ix_b` from local `indexes.json`.
   `rdc sync <env>` **without** `--allow-deletes` (non-interactive) → assert
   it **bails** with the "marked for deletion … `--allow-deletes`" message and
   `ix_b` **still present** remotely; lockfile/base unchanged (re-runnable).
   Then `rdc sync <env> --allow-deletes` → assert `ix_b` dropped remotely.
5. **Admin-added survives (the fix).** Out-of-band, create a NEW regular index
   (`ix_admin`) — not in local, not in base. Make an unrelated local edit so
   the dataset is dirty (e.g. modify `ix_a` options). `rdc sync <env>
   --allow-deletes`. Assert `ix_admin` **survives** (base-aware: ∉base ∧
   ∉local → never a pending delete) while the local edit applied.
6. **Idempotency.** Final `rdc sync <env>` → 0 MDH write ops; `indexes.json`
   byte-stable. Teardown drops the collection.

Edge cases pinned across phases: `_id_` never created/dropped; search-index
async create/drop tolerated via polling; `defs_equivalent` `v`-only no spurious
modify (covered by the "re-sync clean" assertions).

### 4.4 Capture-then-pin goldens

For any output whose exact form is uncertain (the normalized search-index
shape on disk), use the existing `RDC_LIVE_CAPTURE=1` capture-then-review flow
and commit the reviewed golden under `testdata/live/expected/`, rather than
predicting it.

## 5. Constraints (must hold)

- **Idempotent + isolated:** run-id throwaway collection only; teardown +
  janitor; never touch a real collection; re-runnable; "sync twice → 0 ops".
- **No customer identifiers** anywhere (source, tests, fixtures, goldens,
  commit messages): real collection names, org id, host, token — none in the
  repo. All live config from `RDC_LIVE_*` env. Sandbox token is a secret.
- **Default `cargo test` stays green;** live scenarios stay `#[ignore]` and
  skip-with-message when `RDC_LIVE_*` is unset. The `diff_indexes_3way` unit
  tests run by default.
- **Verify-first:** the fix and scenario are live-verified on the sandbox
  before claiming done (per the repo's empirical-verification rule), under
  `script -q` where TTY behavior matters, confirming which `rdc` binary runs.

## 6. Risks / open items

- **Per-dataset gating vs. single aggregate prompt.** The object-delete phase
  prompts once for all kinds; MDH gates per-dataset inside `push_dataset` (the
  per-dataset push architecture). A multi-dataset removal could prompt/bail
  per dataset. Acceptable (multi-dataset simultaneous index removal is rare);
  documented. Revisit only if it bites.
- **Mid-loop bail.** A non-interactive bail on a gated removal aborts the sync
  after earlier datasets already pushed. Each dataset is independent and the
  gated dataset applied nothing before bailing (gate precedes `apply_diff`), so
  there is no partial state within a dataset; re-running resumes. Consistent
  with the global fail-loud delete gate.
- **Search-index async timing.** Atlas create/drop is background; the scenario
  polls with the same generous timeouts the driver uses
  (`SEARCH_DROP_TIMEOUT = 60s`). If the sandbox is slow, bump the poll bound in
  the test, not the driver.
- **No production visibility changes** beyond `IndexSet: Default`. The test
  crate reaches the Data Storage base via the already-`pub`
  `EnvConfig::data_storage_base()` and reuses the already-`pub`
  `DataStorageClient`; only collection-lifecycle endpoints
  (`collections/create`, `insert_one`, `collections/drop`) need a raw helper.
