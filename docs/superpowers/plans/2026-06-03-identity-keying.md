# Identity keying: re-key on stable server `id` — Design + Implementation Plan

## Status — SHIPPED (global, id-pinned slugs)

The fix that shipped is **simpler than the id-keyed-lockfile plan sketched
below**, and was chosen after an adversarial audit showed the id-keyed `nested`
map (explored on this branch, then dropped) was *redundant* and a *new bug
source* (every slug-mutating path — `realign`, deploy write-back, drift-adopt,
url-lookup — would have to maintain a second map; several didn't → split-brain).

Root cause, precisely: queue-nested slugs (`queues`/`schemas`/`inboxes`) were
deduplicated **per workspace** but stored/matched in **one global namespace**
(`lockfile.objects[kind]` + the classifier's `(kind, slug)` keys). Two
same-named queues in different workspaces both claimed the bare slug and
collapsed onto one identity → cross-attribution.

**The fix:** make queue slug assignment **global** (matching the namespace it
is keyed in) and **pinned by id** via the existing `slug_for_id` (sticky local
slug across remote renames). Applied in `pull::queues::process`,
`from_catalog_scan_lockfile`, and both `execute.rs` slug builders, each
pre-seeding the global used-set with already-pinned slugs so a newly-seen queue
never steals one. Two same-named queues now get distinct slugs
(`shared-queue` / `shared-queue-2`), distinct dirs, distinct lockfile entries;
stable across renames and list order. No lockfile format change (still v2).

**This fixes `rdc deploy` too, with no deploy code change:** deploy matches by
the on-disk queue slug, which is now globally unique, so `collect_queue_slugs`'
`dedup()` is a no-op and `locate_queue_dir`'s first-match has a single match.
Verified by `deploy::map::tests::auto_match_keeps_same_named_queues_distinct_across_workspaces`.

**Kept:** the Phase-0 fail-loud collision guard (`src/cli/sync/collision.rs`),
now a defensive backstop that never fires (global dedup makes the collision
impossible). **Not a bug:** `index.rs` lists a per-engine field's bare slug in
the human-readable `_index.md` — unambiguous within its engine, output-only.

Verified live on the TEST API (sync: two same-named cross-workspace queues →
two distinct entries; edit to one queue's schema PATCHed its OWN remote schema,
the other untouched) and by the full test suite.

---

> The sections below are the **original id-keyed-lockfile plan** that was
> explored and **rejected** in favour of the simpler global-dedup fix above.
> Retained for context; not the shipped design.

## Problem (verified)

rdc overloads one string — the **slug** (`slugify(name)`) — to mean three
different things at once:

1. **Identity** — *which remote object is this?*
2. **Location** — *where does it live on disk?* (`workspaces/<ws>/queues/<q>/`)
3. **Label** — *what do we call it?*

The lockfile is really an **identity map** (`slug → server id + base hash`),
but it is keyed by a *mutable, non-unique, name-derived* string whose
uniqueness is enforced in a namespace that does **not** match where it is
stored:

| Concern | Where uniqueness is enforced | Where it is stored / matched |
|---|---|---|
| queue slug | **per workspace** (`per_ws_used_q_slugs`) | **global** `lockfile.objects["queues"]: BTreeMap<slug,_>` and classifier `(kind, slug)` keys |

Two queues named the same in **different workspaces** therefore both become
`shared-queue`, collapse to one identity, and a sync cross-attributes one
queue's `queue.json` / `schema` / formulas / `inbox` onto the other.

**Empirically reproduced** against the TEST API: 28 remote queues → 27 lockfile
entries; a no-edit re-sync was non-idempotent; one apply sync renamed a
second queue's remote schema to the first's.

### Collision matrix (audited, all kinds)

| Kind | Identity key | Dedup scope | Verdict |
|---|---|---|---|
| **queues / schemas / inboxes** | **bare `q_slug`** | **per-workspace** | ❌ collapse + cross-attribute |
| workspaces, hooks, rules, labels, engines, workflows | bare slug | **global** | ✅ safe |
| email_templates, engine_fields, workflow_steps | **composite `<parent>/<child>`** | per-parent | ✅ safe (the correct existing pattern) |
| mdh datasets | dataset slug | global; names unique in store | ✅ safe |
| organization | `"self"` | singleton | ✅ safe |

Two lower-severity issues in the same class:
- **`rdc deploy`**: `deploy/map.rs` `collect_queue_slugs` / `collect_queue_slugs_with_file`
  `dedup()` bare slugs, dropping workspace context → same cross-attribution on deploy.
- **Pagination order-stability**: `api/mod.rs` `list_paginated` uses
  `buffer_unordered`, returning pages in completion order. For *any* same-name
  objects spanning a page boundary, the `-2` suffix is assigned
  non-deterministically on a first pull / `rdc doctor --rebuild-lock`.

## Design principle

> **Identity = server `id`. Slug = a pinned, human-friendly path/label that
> nothing keys on.**

The server already mints a stable, globally-unique `id` for every object. Use
it as the identity everywhere the pipeline matches objects (lockfile,
classifier, scan, remote-hash map, push target resolution). Keep the slug only
for the ergonomic on-disk layout, pinned by id (as `slug_for_id` already
does), and explicitly allowed to be non-unique because no decision depends on
it. This removes the **entire** collision class for **all** kinds at once and
makes identity independent of list order (so the pagination fragility becomes
moot too).

Why id-based and not just composite (`<ws>/<q>`) keys for queues? Composite
keys would fix cross-workspace collisions, but they are still name-derived and
order-fragile, and they do not help same-parent renames or the deploy/order
issues. Composite is the *minimum consistency fix*; id-based is the *complete*
fix and is cheap because the lockfile already records `id` for every entry.

---

## Phase 1 — Identity type + lockfile re-key (v2 → v3)

### Task 1.1: Introduce an explicit identity key

Add `ObjectKey { kind: String, id: u64 }` for tracked (already-synced)
objects, and a `PendingCreate { kind, path }` variant for local files with no
`id` yet. Today the classifier keys on `(kind, slug)`; it will key on
`ObjectKey` for tracked objects and on the create-path for creates.

### Task 1.2: Lockfile v3 — keyed by id, slug as attribute

Change `Lockfile.objects` from `BTreeMap<kind, BTreeMap<slug, ObjectEntry>>`
to `BTreeMap<kind, BTreeMap<id, ObjectEntry>>`, where `ObjectEntry` gains a
`slug: String` field (and keeps `url`, `modified_at`, `content_hash`,
`secrets_hash`). `LOCKFILE_VERSION = 3`.

- `load()` migrates v2 → v3 mechanically: every v2 entry already has `id`, so
  re-key `slug → id` and move the old key into `entry.slug`.
- **Migration detects the historical collision:** if two v2 slugs share an id
  (can't happen) — or two ids would land under the same slug on disk — record
  both under their ids; the doctor repair (Phase 3) reconciles disk paths.
- Helpers: `entry_for_id(kind, id)`, `slug_for_id` (now a direct lookup),
  `id_for_slug(kind, slug)` (scan — used only at the scan boundary).

### Task 1.3: Centralize the id ↔ (slug, path, url) mapping

The bug exists because pull, sync-remote-hash, scan, and the codec each
*re-derive* the key independently and drifted (codec assumed composite; the
rest used bare). Introduce one `ObjectIndex` that owns this mapping for a sync
run; every stage consumes it. **No stage re-derives identity.**

---

## Phase 2 — Pipeline cutover to id identity

### Task 2.1: Pull — record by id

`record_object` keys by `(kind, id)`; the slug is computed for the path and
stored as an attribute. Two same-name queues in different workspaces now
produce **two** entries (ids differ) — no collapse.

### Task 2.2: Scan — recover identity from the on-disk `id`

Each synced JSON already contains `id`; the scanner reads it to map a file →
`ObjectKey`. A file with no `id` (or `id: null`) is a `PendingCreate`. This
replaces the `find_queue_nested_path` *first-match sweep* (the source of the
"wrong file → wrong remote id" hazard) with an exact id→path lookup.

### Task 2.3: Classify + remote-hash map — key by id

`classify` and the `remote_hashes` / `locked` maps key on `ObjectKey`.
Creates keyed by path. The eleven-class truth table is unchanged; only the key
type changes.

### Task 2.4: Push/execute — resolve target by id

Push PATCH/POST/DELETE resolves the remote object by `entry.id` directly (no
slug→id round-trip), eliminating the cross-attribution path entirely.

### Task 2.5: Delete the Phase-0 guard's necessity

Once identity is id-based, the collision cannot occur. Keep the
`CollisionDetector` as a cheap invariant assertion (it should now *never*
fire) or downgrade it to a debug-assert. Keep its tests.

---

## Phase 3 — `rdc doctor` repair for already-collided lockfiles

A user who synced a colliding org on an old binary has a corrupted v2
lockfile (one entry where there should be two) and possibly cross-written
remote objects. `rdc doctor` gains a `repair-identity` step:

1. List remote; detect ids whose on-disk dir/slug collapsed (two dirs, one
   lockfile id, or a lockfile id not matching the dir's `queue.json` id).
2. Re-derive a distinct slug/path per id (workspace-qualified), move the
   on-disk dir if needed, and rebuild the lockfile entry per id.
3. Report any remote object whose content no longer matches its id's history
   (possible prior cross-write) so the user can review — never auto-overwrite
   remote.

---

## Phase 4 — Fix `rdc deploy` mapping

`deploy/map.rs`: `collect_queue_slugs` / `collect_queue_slugs_with_file`
must carry workspace context (key by `<ws_slug>/<q_slug>` or by id), and
`locate_queue_dir` must resolve the specific workspace, not the first match.
Mirror the id-based identity from Phases 1–2 in the deploy mapping tables.

## Phase 5 — Deterministic pagination

`api/mod.rs` `list_paginated`: collect `buffer_unordered` results into a
`Vec<(page_no, items)>` and sort by `page_no` before flattening (or sort the
final list by `id`). Removes the first-pull / `--rebuild-lock` slug-assignment
non-determinism for any transitional name-derived suffixes.

---

## Verification

- **Unit:** identity re-key, v2→v3 migration round-trip, scan id-recovery,
  classify on `ObjectKey`, doctor repair.
- **Integration (wiremock):** the cross-workspace same-name scenario from
  `tests/cli_sync.rs::sync_errors_on_cross_workspace_queue_name_collision` —
  after Phases 1–2 it must **succeed** and produce **two** distinct lockfile
  entries / dirs (not error, not collapse). Add a same-workspace same-name
  case and a deploy cross-workspace case.
- **Live (TEST API):** recreate two same-name queues in two workspaces, edit
  one's schema, sync, and assert each queue's remote schema is updated
  independently (no cross-write). This is the scenario already used to prove
  the bug and the Phase-0 guard.
- Full `cargo test` green (note: `snapshot::codec::hooks::tests::disk_json_contains_sentinel_not_ready`
  is a **pre-existing**, unrelated failure — fix or quarantine separately).
