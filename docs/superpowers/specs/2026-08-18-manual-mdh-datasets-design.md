# Manual MDH datasets — versioned row data

**Status:** design, awaiting review
**Date:** 2026-08-18

## Problem

rdc snapshots MDH (Master Data Hub) *structure* only — the collection name
(`collection.json`) and its index definitions (`indexes.json`). Row data is
deliberately out of scope (`src/api/data_storage.rs:7-15`).

That is correct for datasets fed by an import pipeline: their rows are large,
volatile, and environment-specific, and re-loading them is the import hook's
job. It is wrong for **manually maintained datasets** — small lookup and
configuration tables (synonym lists, GL-code maps, unit conversions) that a
human types into the MDH UI. Those rows *are* configuration:

- they are not reproducible from any upstream system;
- losing them means re-typing them;
- promoting a change from `dev` to `test` to `prod` today means re-typing it
  in every environment, with no review and no diff.

**Goal:** let a dataset be declared *manual*, so its rows are pulled into the
snapshot, versioned in git, reviewed like any other file, and replicated to
other environments by the existing `rdc migrate` + `rdc sync` workflow.

**Non-goal:** managing import-fed datasets. rdc must keep ignoring their rows.

## Verified facts

Nothing below is inferred. API facts come from live probes against a throwaway
collection (`rdc_probe_*`) on a sandbox org; code facts cite the tree.

### Data Storage API (probed live 2026-08-18)

| # | Fact | Consequence |
|---|---|---|
| A1 | `POST /v1/data/find {collectionName, query:{}}` returns every document in one response. Observed: 1206 docs in a single body; 1002 docs ≈ 442 KB in 0.35 s. | One `find` per dataset is enough at the target scale. No paging. |
| A2 | `_id` is returned as EJSON `{"$oid":"<hex>"}` for server-generated ObjectIds, and as a plain scalar (e.g. `"natural-key-1"`) when the document was inserted with an explicit `_id`. | The `$oid` wrapper is a reliable marker of *server-generated* identity — the one field safe to strip. |
| A3 | `POST /v1/data/insert_many` **accepts an explicit `_id`**, in both `{"$oid":…}` and plain-string form; the response echoes them in `result.inserted_ids`. | Row identity can be carried across environments. |
| A4 | Re-inserting documents returned by `find` verbatim into a *different* collection round-trips **byte-identically**. Verified across: `i64` > 2⁵³ (`9007199254740993`), floats, `true`, `null`, non-ASCII (`Přílöhá — ünïcode`), **trailing whitespace** (`"sep "`), `{}`, `[]`, nested objects/arrays, EJSON `{"$date":…}`, **dotted field names** (`"dotted.key"`), and `$`-prefixed *values* (`"$gt"`). | The on-disk form can be the service's own JSON. No lossy transform, no type registry, no escaping scheme. |
| A5 | Inserting a document whose `_id` already exists → HTTP 400 `{"code":"error","message":"batch op errors occurred"}`. With `ordered:false`, the **non-conflicting documents in the same batch are still inserted**. | Never blind-insert. A failed write is *partially applied*: on error, re-read rather than assume a no-op. |
| A6 | `POST /v1/data/delete_many {filter:{"_id":{"$in":[…]}}}` works with a **mixed-type** id list (`$oid` objects and plain strings in one array); returned `deleted_count: 3`. | The targeted delete leg is sound. |
| A7 | `POST /v1/data/delete_many {filter:{}}` is permitted; returns `result.deleted_count`. | Mass delete is possible (we choose not to use it — see D3). |
| A8 | `POST /v1/data/bulk_write` → HTTP 202 `{"code":"accept","message":""}`. **The `message` is empty — no operation id.** (The vendor reference doc states it carries one; that is false on this deployment.) | Operation-status polling is impossible ⇒ use the synchronous verbs (`insert_many`, `delete_many`, `replace_one`), which return HTTP 200 with counts. |
| A9 | Every write was visible to an immediately following `find`, with and without `waitForFullWrite:true`. | sync's same-cycle MDH pull-back (`execute.rs:3853`) will not read stale rows. We still pass `waitForFullWrite:true` on writes — belt and braces, no measured cost. |
| A10 | `find` against a **nonexistent** collection returns `{"code":"ok","result":[]}` — no 404. | Existence must come from `collections/list`. An empty `find` never implies "collection gone". |
| A11 | `collections/list` (`nameOnly:false`) carries no row count (`name`, `type`, `options`, `info`, `idIndex` only). `POST /v1/data/aggregate [{"$count":"n"}]` returns `[{"n":N}]`, and `[]` for an empty collection. | The size guardrail needs one cheap `aggregate` call; treat `[]` as 0. |
| A12 | Natural (unsorted) `find` order is **not** stable across calls: two queries differing only in `projection` returned the same documents in different orders. | On-disk row order must be imposed locally. |
| A14 | `POST /v1/data/replace_one` (standalone, no `bulk_write`) returns HTTP 200 with `matched_count` / `modified_count` / `upserted_id`. A replacement **omitting** `_id` keeps the filter's id; including the *same* `_id` also works; a no-match without `upsert` leaves the collection untouched (`matched_count: 0`, no insert). | The in-place replace leg is sound and safe: a stale filter cannot silently create a row. |
| A15 | A replacement that **changes** `_id` is rejected: HTTP 400 `"After applying the update, the (immutable) field '_id' was found to have been altered"`. | rdc always sends the replacement with `_id` omitted, which makes this error class unreachable. |
| A16 | `insert_many` accepts `ordered:false` and `waitForFullWrite:true` together (HTTP 200). | The insert leg's exact call shape is verified as used. |
| A13 | A real import-fed collection carries a per-row **`__digest_md5`** field (the MDH import extension's differential-sync digest). Other real collections hold runtime output (`annotation_id`, `created_at`, …) or unrelated app state with a string `_id`. | Confirms opt-in-only, and justifies a warning when a dataset flagged manual carries `__digest_md5`. |

### rdc code facts

| # | Fact | Cite | Consequence |
|---|---|---|---|
| C1 | `canonicalize_for_hash` returns the raw bytes unchanged when the input is not JSON. | `snapshot/noise.rs:57-60` | A `.jsonl` file is hashed **byte-for-byte**. Its on-disk form must be canonical, or every pull rewrites it. |
| C2 | `decide_pull_action` / `apply_pull_action` operate on opaque bytes + hashes. | `cli/pull/common.rs:568,685` | The whole three-way pull machinery (Write / NoChange / KeepLocal / Conflict, shadow files, `--conflict`) is reusable for `data.jsonl` as-is. |
| C3 | `base_cache::cache_mirror` is a pure path mirror; `prune_orphans` prunes any cache file lacking an env-tree counterpart, with no kind knowledge. | `state/base_cache.rs:44-48,120-143` | Base-cache support for the new file is automatic. |
| C4 | `pull::mdh::process` rewrites `collection.json` from server truth whenever the bytes differ. | `cli/pull/mdh.rs:500-509` | **Latent blocker:** a hand-added manifest field would be erased on the next pull. The writer must merge, not clobber. |
| C5 | `migrate::is_managed_leaf` accepts only `json`, `py`, `js`. | `cli/migrate/mod.rs:1643-1648` | `data.jsonl` would be silently *not* migrated. Must be extended. |
| C6 | `migrate::owning_object` maps every leaf under `mdh/<slug>/` to `("mdh", slug)`. | `cli/migrate/mod.rs:852` | `--mirror` prune and per-object status accounting already treat a new dataset leaf correctly. |
| C7 | Non-JSON files take migrate's verbatim-copy path, which honours an overlay shadow at `envs/<tgt>/overlay/<relpath>`. | `cli/migrate/mod.rs:589-601` | Per-env row-data override comes for free. |
| C8 | Migrate writes JSON as `to_vec_pretty(value) + "\n"`, the same shape as `collection_manifest_bytes`. | `cli/migrate/mod.rs:779-781`, `cli/pull/mdh.rs:24-29` | A manifest carrying the new flag migrates byte-stably. |
| C9 | MDH bypasses the sync classifier; `classify.rs` contains no MDH logic, and no code sweeps `lockfile.objects` generically. | `cli/sync/classify.rs`, grep | A new lockfile kind is contained — it cannot leak into unrelated classification. |
| C10 | `remove_mdh_dataset` deletes the dataset dir, forgets the `indexes.json` base mirror, and drops the `mdh_indexes` lockfile entry. | `cli/sync/execute.rs:2046-2060` | Must also forget the `data.jsonl` mirror and drop the new lockfile entry. |
| C11 | Under an active `--only`, migrate skips MDH files entirely (`classify_for_selection` returns `None` for mdh). | `cli/migrate/mod.rs:2053-2060` | Pre-existing gap, documented as a known limitation; not widened here. |
| C12 | `serde_json` is built with `preserve_order` and **without** `arbitrary_precision`. | `Cargo.toml:33` | Key order is whatever we impose; numbers round-trip through `i64`/`u64`/`f64` (A4 confirms this is lossless for realistic data). |

## Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | **Opt-in via `collection.json`**: `{"name": "GL_CODES", "data": "manual"}`. Absent ⇒ today's behavior exactly. | Self-describing; travels through `migrate` for free (C8); no new config surface. |
| D2 | **On-disk form `data.jsonl`** — one document per line. | Best git diffs: one row = one line. |
| D3 | **Local file is authoritative**, executed as a **hybrid keyed diff** (below), never as delete-all-then-reinsert. | Makes the env match the file without an empty-collection window and without churning the ObjectIds of untouched rows. |
| D4 | **Size ceiling:** warn above 1 000 rows, hard error above 10 000 — applied in **both** directions (the remote count on pull, the local line count on push). | Keeps a huge import-fed table from being committed by accident, with a forgiving middle band. |
| D5 | **Synchronous verbs only** (`find`, `insert_many`, `delete_many`, `replace_one`). No `bulk_write`. | A8: `bulk_write` yields no operation id, so its completion is unobservable. |

## On-disk format

```
envs/<env>/mdh/<slug>/
├── collection.json     {"name": "GL_CODES", "data": "manual"}
├── indexes.json        (unchanged)
└── data.jsonl          NEW — one document per line
```

`data.jsonl` canonical form, in full:

1. One JSON object per line, **compact** (no insignificant whitespace).
2. Object keys sorted lexicographically, **recursively** at every nesting level.
3. Lines sorted by their canonical bytes (byte-lexicographic). Because `_` (0x5F)
   sorts before every lowercase letter, a row carrying `_id` naturally sorts by it.
4. UTF-8, LF endings, trailing newline. A manual dataset with zero rows is a
   **0-byte file** — which is what distinguishes "manual and empty" from "not manual".
5. `_id` is **dropped** when and only when its value is an object whose single key
   is `$oid` (A2 — server-generated identity). Any other `_id` (string, int, …) is
   preserved verbatim: it is a business key the user authored. This rule is applied
   **identically to both sides** — rows read from the env and rows read from
   `data.jsonl` — so a hand-written `{"_id":{"$oid":…}}` in the file is ignored for
   comparison and removed the next time the file is written. Without that symmetry
   the two sides could never compare equal and the dataset would churn forever.
6. Every other field is preserved verbatim, including `__digest_md5` if present
   (it is data; see the warning below).

Rules 1–3 exist because a `.jsonl` file hashes byte-for-byte (C1): without a
single canonical form, pull and push would rewrite the file forever.

Two documented consequences:

- **MDH UI column order.** Sorting keys means rows that rdc *creates or replaces*
  are stored with sorted field order, which is the order the MDH UI displays.
  Rows rdc never touches keep their original order — comparison is on the
  canonical form, so a pure order difference is not a change and triggers no write.
- **Row order in the file** is canonical, not insertion order. Adding a row lands
  it in sorted position.

## Diff engine (push)

Given local rows `L` (parsed from `data.jsonl`) and remote rows `R` (one `find`),
both reduced to canonical form by the *same* function. For each remote row the
implementation retains its **raw** `_id` alongside the canonical form — that raw
id is what the delete leg targets (A6), and it is the only thing the canonical
form discards.

```
partition by identity:
  keyed(x)   = x has an explicit _id (i.e. not a $oid object)   -> key = _id
  unkeyed(x) = otherwise                                        -> key = canonical bytes

keyed:
  in L only            -> insert_many   (carrying the explicit _id)
  in both, differing   -> replace_one {_id: <id>}      identity preserved, no delete
  in both, equal       -> no call
  in R only            -> delete_many  {_id: {$in: [...]}}

unkeyed (multiset by canonical bytes; counts matter):
  count(L) > count(R)  -> insert_many the surplus
  count(L) < count(R)  -> delete_many the surplus, by their raw remote _id
  equal                -> no call
```

Order of operations: **deletes first, then replaces, then inserts.** Deleting
before inserting is what lets a unique index survive a row edit — the outgoing
row releases its key before the incoming row claims it.

`replace_one` is always sent with `_id` **omitted** from the replacement body: the
filter carries the identity, and omitting it makes the "immutable `_id` altered"
rejection (A15) unreachable by construction. A `matched_count: 0` response means
the row vanished between the read and the write — treat the push as not fully
applied and let the next sync re-diff (A14 guarantees nothing was created).

**Safety rule — an absent `data.jsonl` is never authoritative.** A dataset flagged
manual whose `data.jsonl` does not exist yet (flag added by hand, never pulled) is
"not yet pulled", *not* "the env should have zero rows". Push skips it entirely;
the pull creates the file first. Only a present file — including a deliberately
0-byte one — expresses "these are all the rows".

Chunking: 500 documents per `insert_many`, 500 ids per `delete_many` (A1/A4
measured 1 000 docs ≈ 440 KB at 1.07 s, so 500 is comfortably inside limits).

Deletion gate: reuse the existing `classify_delete_gate` from
`push/mdh.rs` — `--allow-deletes` proceeds, an interactive TTY prompts with the
row count, non-interactive without the flag bails with an actionable message.
This matches how index drops already behave.

Duplicate local lines are legal (multiset) but warn: in a lookup table they are
almost always a mistake.

## Pull

A new sub-phase in `pull::mdh::process`, per dataset, **only when the manifest
says `"data": "manual"`** (so a non-manual dataset makes zero extra API calls —
important, since `plan_mdh_index_edits` already costs one fetch pair per dataset):

1. `aggregate [{"$count":"n"}]` → row count. Warn > 1 000; hard error > 10 000,
   naming the dataset, the count, and how to opt out (D4, A11).
2. `find {query:{}}` → all rows (A1).
3. Warn once if any row carries `__digest_md5` — the dataset is import-managed and
   rdc's authoritative pushes will fight the import hook (A13).
4. Canonicalize to `data.jsonl` bytes.
5. Feed the existing three-way machinery (C2) — `decide_pull_action` /
   `apply_pull_action` — keyed under a new lockfile kind **`mdh_data`** (dataset
   slug → content hash), with a base-cache mirror (C3). Conflicts, shadow files,
   `--conflict use-remote|keep-local|skip`, and the `KeepLocal` base-preservation
   rule the index path already implements all come along unchanged.

## Manifest merge (fixes C4)

`collection_manifest_bytes(name)` is replaced by a merge writer:

- read the existing `collection.json` (if any) as a `serde_json::Map`;
- set `name` to server truth, **preserving every other key and the file's own key
  order** (C12: `preserve_order`);
- serialize `to_vec_pretty` + `\n` (C8, so bytes stay comparable with migrate's).

For a manifest that contains only `name`, output is byte-identical to today — no
churn for existing projects. An unrecognised `data` value is a hard error naming
the file and the accepted values, so a typo (`"manual "`, `"Manual"`) fails loudly
instead of silently reverting the dataset to metadata-only.

## Sync integration

Extends the existing four MDH stages in `sync/execute.rs:3688-3882`:

| Stage | Today | Added |
|---|---|---|
| 1. drift push (existing collections) | pushes `indexes.json` drift | also pushes `data.jsonl` when its hash ≠ the `mdh_data` baseline |
| 2. create (collection absent on env) | creates collection + indexes | then loads rows from `data.jsonl` — **this is the replication path** |
| 3. pull-back | re-reads indexes | also re-reads rows (idempotent by construction) |
| 4. orphan prune | removes dir, forgets `indexes.json` mirror, drops `mdh_indexes` | also forgets the `data.jsonl` mirror and drops `mdh_data` (C10) |

`--dry-run`:

- `plan_mdh` (network-free) gains, for manual datasets: `mdh/<slug> data PATCH`
  when the local hash differs from the baseline, and `mdh/<slug> data (new)` when
  the dataset has no local `data.jsonl` yet.
- `plan_mdh_index_edits` (network-backed) gains a row-body forecast — one `find`
  per manual dataset — reusing the same `index_edit_item` decision shape so the
  preview cannot disagree with the real run.

`--no-push` suppresses the data push and keeps the data pull; `--no-pull` pushes
the rows but never rewrites `data.jsonl` — exactly as both flags already behave
for indexes.

## Migrate integration

- `is_managed_leaf` gains `jsonl` (C5). This is the only change needed for
  replication: the file is copied verbatim, `owning_object` already attributes it
  to `("mdh", slug)` (C6), so `--mirror` prunes it with its dataset and the summary
  counts the dataset once.
- Verbatim copy means an overlay shadow at
  `envs/<tgt>/overlay/mdh/<slug>/data.jsonl` overrides the migrated rows for that
  env (C7) — a per-env data override, documented as supported.
- Promotion flow, end to end: `rdc migrate dev test` copies `collection.json`
  (with the flag) plus `data.jsonl`; `rdc sync test` creates the collection if
  absent, applies the indexes, then applies the row diff.

## New API surface

`DataStorageClient` gains five methods (module doc updated — its current text
states row-data writes are intentionally absent, which this design changes
deliberately):

| Method | Endpoint | Notes |
|---|---|---|
| `find_all` | `POST /v1/data/find` `{query:{}}` | A1 |
| `count` | `POST /v1/data/aggregate` `[{"$count":"n"}]` | `[]` ⇒ 0 (A11) |
| `insert_many` | `POST /v1/data/insert_many` | `waitForFullWrite:true`, `ordered:false`; caller chunks (A5, A9) |
| `delete_many_by_ids` | `POST /v1/data/delete_many` `{"_id":{"$in":[…]}}` | mixed id types OK (A6); returns `deleted_count` |
| `replace_one` | `POST /v1/data/replace_one` | filter `{_id}`, replacement with `_id` omitted, no upsert; returns `matched_count` (A14, A15) |

All are synchronous 200-with-counts responses (D5); the existing
`post_envelope` / `post_envelope_void` helpers cover them, including the
`code == "ok" | "accept"` validation.

## Backward compatibility

| Scenario | Behavior |
|---|---|
| Existing project, no dataset flagged | Byte-identical to today: no `data.jsonl`, no `mdh_data` lockfile entries, **zero additional API calls**. |
| Existing `collection.json` (only `name`) | Manifest writer emits identical bytes; no churn. |
| Old rdc binary meets a project with manual datasets | It ignores `data.jsonl` (unknown leaf) and, on its next pull, rewrites `collection.json` without the flag — degrading the dataset to metadata-only. The data file survives in git; re-adding the flag restores the behavior. Documented in the README; `rdc upgrade` is the fix. |
| New rdc binary, dataset flagged, remote collection absent | Stage 2 creates it and loads the rows. |
| New lockfile kind `mdh_data` | Registered alongside `mdh_indexes` as an id-`0` sentinel kind (`lockfile.rs:238,520-548`) and excluded from `refs::is_portable_kind` (`snapshot/refs.rs:17-21`), matching `mdh_indexes`. Contained: nothing sweeps lockfile kinds generically (C9). |
| A flagged dataset that is actually import-fed | Warned at every pull (`__digest_md5`, A13). rdc still does what it was told. |

## Failure modes

| Failure | Handling |
|---|---|
| Insert rejected (duplicate `_id`, A5) | The batch is *partially applied*. Do not advance the lockfile/base; surface the API message with the dataset and the offending id count; the next sync re-reads remote and re-diffs, so the retry is self-correcting. |
| Collection missing mid-run | `find` returns `{"result":[]}`, not 404 (A10) — so existence is decided from the `collections/list` catalog, never from an empty read. |
| Row count over ceiling | Hard error before any write, naming the count and the opt-out (D4). |
| Push partially applied (some chunks landed) | Lockfile/base are advanced **only** on a fully-applied push — the same `fully_applied` rule the index path uses (`push/mdh.rs:207`). A partial push stays "local diverged" and retries next sync. |
| Both sides edited the rows | Standard `BothDiverged` handling via C2. For `.jsonl` the strict sidecar merge applies (one side changed ⇒ take it; both ⇒ interactive resolver / shadow file). No line-level auto-merge in v1. |

## Testing

Hermetic (run under plain `cargo test`):

- canonicalization: key sort recursion, line sort, `$oid` strip vs business-key
  preserve, 0-byte empty dataset, idempotence (canonical in ⇒ identical out),
  and the A4 type matrix (i64 > 2⁵³, float, null, unicode, trailing whitespace,
  empty containers, `$date`, dotted keys) as a round-trip property test;
- diff engine: keyed insert/replace/delete, unkeyed multiset surplus both ways,
  no-op on equality, delete-before-insert ordering, duplicate-line warning;
- manifest merge: flag preserved across a pull that rewrites `name`; byte-identity
  for a `name`-only manifest; hard error on an unknown `data` value;
- wiremock pull/push drivers: guardrail warn + hard error, `__digest_md5` warning,
  three-way outcomes, `fully_applied` gating;
- `plan_mdh` / `plan_mdh_index_edits` forecasts match the executed run;
- migrate: `data.jsonl` is copied, overlay shadow overrides it, `--mirror` prunes
  it with the dataset.

Live (`tests/live/scenarios/mdh.rs`, opt-in `RDC_LIVE_*`, throwaway
`rdc_it_<run>_mdh` collection): pull rows → hand-edit the file → sync → verify
remote rows → **idempotent re-sync (0 ops)** → replicate to a second throwaway
collection and assert equality → gated delete path.

## Known limitations

- `rdc migrate --only mdh/<slug>` selects nothing, because `classify_for_selection`
  returns `None` for MDH (C11). Pre-existing; unchanged here.
- No line-level three-way merge for `data.jsonl` in v1 (strict sidecar semantics).
- Datasets fed by an import hook are out of scope by design; flagging one is
  warned about, not prevented.
