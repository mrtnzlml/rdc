# Deferred Relink for Cyclic References in `rdc sync` Push

- **Date:** 2026-06-17
- **Status:** Approved design — pre-implementation
- **Scope:** `rdc sync` push pipeline (`src/cli/sync/execute.rs`, `src/cli/push/*`, `src/snapshot/refs.rs`)

## 1. Problem

`rdc sync <env>` aborts mid-push when a queue references an object that does
not yet exist in the target environment:

```
fail   pushing queues for env 'test-ops': PATCH /queues/5550440:
       Rossum API returned status 400: {"engine":["Invalid hyperlink - No URL match."]}
```

The queue's `engine` is a portable ref `rdc://engines/1-intake-triage-ops`
pointing at an engine that is itself a `LocalCreate` in the same sync.

## 2. Root cause (verified)

1. **Push order.** `push_classified` (`src/cli/push/mod.rs`) dispatches queues
   (~line 53) **before** engines (~line 80). The queue PATCH runs while the
   referenced engine has no lockfile id.
2. **Dangling-ref passthrough.** `refs::resolve_value` rewrites only refs whose
   slug is in the lockfile; a dangling ref is left verbatim "so the API
   surfaces a clear error" (`src/snapshot/refs.rs`). `engine` rides in the
   queue's flattened `extra` and is not in `strip_patch_extra`, so the literal
   `rdc://…` string reaches the wire.
3. **A true cycle, not just ordering.** `queue.engine → engine` and
   `engine.training_queues → queue` are mutually referential (`training_queues`
   is a *writable* field). Some queues and the engines that train on them are
   both new in one sync, so **no linear push order satisfies both edges** — the
   cycle must be broken by creating skeletons and relinking afterward.
4. **The dependency is 3-deep (verified live).** Binding `queue.engine = E`
   requires `E` to already own `engine_fields` covering the queue schema's
   extracted fields, or the API returns
   `400 non_field_errors: "Engine (id) restriction: extracted field '<x>' is
   not present among names of engine fields"`. So the real order is
   `engine → engine_fields → queue.engine`, plus `engine.training_queues → queue`.

### Live verification (2026-06-17)

- `queue.engine = "rdc://engines/1-intake-triage-ops"` → exactly
  `400 {"engine":["Invalid hyperlink - No URL match."]}`. A well-formed but
  missing URL instead returns `"Object does not exist."` — so **"No URL match"
  proves the unresolved `rdc://` *scheme* reached the API**, not a stale real URL.
- Engine skeleton `POST` (`training_queues:[]`) → **201**; `DELETE` → **204**
  (engines creatable on test-ops; 403 on org 1 / sandbox 214757).
- `PATCH engine.training_queues:[<real queue url>]` → **200** (the relink
  primitive works).
- `PATCH queue.engine = <fresh engine>` → **400** with the `engine_fields`
  restriction above.
- All probes self-cleaning; target env left byte-identical (1 engine, id 383).

## 3. Goals / Non-goals

**Goals**
- A `sync` whose snapshot contains engine↔queue cycles completes: skeletons are
  created, then cross-references are relinked once every object exists.
- No unresolved `rdc://` is ever sent to the API (shipped guard enforces this).
- Idempotent: a second `sync` with no local changes is a no-op.
- When a reference genuinely cannot be satisfied, fail loud naming the exact
  `(object, field, ref)` — after applying all satisfiable work.

**Non-goals**
- Fixing the **stale-lockfile-id** contamination (§7). The relink resolves
  `rdc://queues/…` through the lockfile, so correct ids are a *prerequisite*,
  tracked separately.
- Deciding whether the snapshot's 6 new per-queue engines *should* exist in
  test-ops (a project/data question, not a tool bug).
- Creating `engine_fields` content that satisfies an engine's binding
  restriction — that is user-authored snapshot data; the relink only sequences
  existing snapshot objects.

## 4. Design

### 4.1 Detection (shipped)
- `refs::residual_rdc_refs(&Value) -> Vec<String>` — every remaining `rdc://`
  ref, sorted/deduped (`src/snapshot/refs.rs`, with tests).
- `api::ensure_no_residual_refs(path, body)` wired into `post_json`/`patch_json`
  — hard backstop: a residual `rdc://` can never reach the API; the error names
  the path and ref(s). (Already implemented + green.)

### 4.2 Deferral (per-kind drivers)
After `resolve_value`, a driver strips any **top-level field whose value still
contains a residual `rdc://`** (scalar or inside an array) and records it:

```
struct DeferredRelink { kind: String, slug: String, fields: Vec<(String, Value)> }
```

The skeleton body — now ref-clean — is created/patched exactly as today, so the
object gets a lockfile id. Implemented as a shared helper
(`resolve_value` → defer) so every driver participates uniformly. The push
order is **unchanged** (no reorder). Under that order the only field that
actually defers is **`queue.engine`** (queues are pushed before engines, so the
engine row doesn't exist yet); `engine.training_queues` resolves *inline* at
engine-create because all queues were already created earlier in the same pass.
The mechanism is generic, so it would also defer `training_queues` (or
`dedicated_engine`/`generic_engine`) should a future ordering leave its target
uncreated — no per-field special-casing.

### 4.3 Relink phase (placement)
Runs in `src/cli/sync/execute.rs` **after `push_classified` (all kinds:
engines, engine_fields, queues, …) and before the existing `portabilize_refs`
post-pass**. Running after *every* create is what satisfies the 3-deep
dependency: by relink time engines, their engine_fields, and all queues exist.

For each `DeferredRelink`:
1. Re-resolve each field's value via `resolve_value` against the now-complete
   lockfile. If any `rdc://` remains → record an unresolved failure (do not send).
2. PATCH the object (`/{endpoint}/{id}`) with a body of just the resolved
   fields. On API error (e.g. the `engine_fields` restriction 400) → record a
   rejected failure with the API message.
3. On success, `record_object` so the `portabilize_refs` post-pass rewrites the
   field to `rdc://` form and re-records the lockfile hash (idempotency).

### 4.4 Error model — apply-all-then-fail-loud
- Engine `CREATE` is made non-fatal on `403/405` (skip + warn), mirroring the
  existing PATCH-`405` handling in `push/engines.rs`, so a plan that forbids
  engine creation does not abort before the relink.
- The relink phase never aborts mid-way; it accumulates failures. After the
  phase, if any failures exist, return one aggregated error listing every
  `(object, field, ref / api-message)`. Exit non-zero. All satisfiable creates,
  patches, and relinks have already been applied.

### 4.5 Idempotency
Relink writes go through `record_object` + the existing `portabilize_refs`
post-pass, which rewrites URLs back to `rdc://` and re-records `content_hash`.
A second `sync` with no local edits classifies every object `Clean` → no-op.

## 5. Data flow (happy path)

Push order is the existing one in `push_classified` (workspaces → schemas →
queues → … → engines → engine_fields). `queue.engine` defers because engines
come later; `engine.training_queues` resolves inline because queues came earlier.

```
push_classified (unchanged order):
  queues       PATCH/POST      (engine DEFERRED — referenced engine not created yet;
                               PATCH omits the field, leaving any existing binding intact)
  ...
  engines      POST            (training_queues resolves INLINE — queues already exist)
  engine_fields POST           (engine now owns the schema's extracted fields)

relink phase (after push_classified, before portabilize_refs — every object
now exists + is lockfile-pinned, so the engine has its fields):
  PATCH queue/<id>   { engine: <resolved engine url> }   -> 200 (bind succeeds)

portabilize_refs post-pass: rewrite URLs -> rdc://, re-hash -> next sync Clean
```

(If a future snapshot also leaves `engine.training_queues` pointing at a
not-yet-created queue, the same generic deferral routes it through the relink
phase as a `PATCH engine/<id> { training_queues: […] }`.)

## 6. Testing
- **Unit (pure):** deferral split (defers a field with residual `rdc://`, keeps
  resolved fields, returns the value for later); re-resolution round-trip
  (defer → upsert target → resolve → fully resolved); fail-loud aggregation
  builds the expected report. Builds on existing `residual_rdc_refs` tests.
- **Live-verified primitives:** engine skeleton create, `training_queues` PATCH,
  the `engine_fields` binding restriction (documented in §2).
- **Idempotency:** after a relink sync, a re-run classifies all objects `Clean`.

## 7. Out of scope / prerequisites
- **Stale lockfile ids (blocker).** test-ops's lockfile records
  `2-paper-and-toner-ops=555325` / `3-stapler-ops=555326`, but the live ids are
  `5550441` / `5550442` (the stale ones 404). The `training_queues` relink
  resolves `rdc://queues/…` through the lockfile, so it would resolve to a dead
  URL until the lockfile is re-pinned (`doctor --rebuild-lock` or equivalent).
  Likely identity-collision contamination. Tracked separately; the relink
  assumes correct ids.

## 8. Open questions
- Should the relink batch all fields of one object into a single PATCH (yes —
  fewer calls, atomic per object).
- Whether `dedicated_engine`/`generic_engine` (currently null in the failing
  project) need the same deferral — the generic mechanism covers them for free.
