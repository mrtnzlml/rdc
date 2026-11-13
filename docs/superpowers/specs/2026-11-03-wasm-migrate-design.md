# Migrate in the browser, and an env tree that survives a closed tab — design

## Intent

The Chrome extension's rdc app builds a `Project` in memory with two envs,
`source` and `target`, in two different orgs. It reads both with
`sync(noPush)`, runs `migrate source target`, shows the plan, then runs
`sync target` to push. A browser tab can close at any moment, and the
in-memory tree is then gone.

This lifts the non-goal "promotion between environments" of
`2026-10-26-wasm-browser-design.md` for `migrate` (deploy stays out), and
extends `2026-10-27-crash-safe-sync-design.md` §6 (the Data Storage state
store). The owner's four decisions are binding; the rest is decided here.

## Public API (the contract the extension builds against)

```ts
// Replaces useDataStorageState (removed 2026-11-12, see §0): the host stores
// each env's state; rdc decides when and what.
export interface StoredState { rev: number; snapshot: Uint8Array; }
export interface StateStore {
  load(env: string): Promise<StoredState | null>;
  save(env: string, snapshot: Uint8Array, expectedRev: number | null, nextRev: number): Promise<boolean>;
}
// Throws unless `store` has exactly the functions `load` and `save`.
useStateStore(store: StateStore): void;

export type CarryGroup = "scoreThresholds" | "emailPrefixes" | "automation" | "all";
export interface MigrateOptions {   // every field may be left out
  dryRun?: boolean;         // default false: print and return the plan, write nothing
  mirror?: boolean;         // default false: also prune target-only objects
  allowRecreate?: boolean;  // default false; needs mirror (throws otherwise)
  only?: string[];          // `--only` selectors, e.g. ["hooks/*", "queues/invoices"]
  carry?: CarryGroup[];     // `--carry` groups; default none
}
export type FileChange = "create" | "update" | "unchanged";
export type ObjectChange = "create" | "update" | "unchanged" | "delete";
export interface MigrateFile {
  source: string;         // project key, e.g. "envs/source/hooks/a.json"
  target: string;         // project key, e.g. "envs/target/hooks/a-test.json"
  change: FileChange;
  kind: string | null;    // the TARGET object the file belongs to, as the sync
  slug: string | null;    // plan and `skipped` name it; null for a file of no object
}
export interface MigrateObject {
  kind: string;           // "hooks", "queues", "mdh", "workflows", ...
  slug: string;           // the TARGET slug (compound for nested kinds)
  change: ObjectChange;   // what this run did to its files in the TARGET TREE (see below)
}
export interface MigratePruned {
  target: string;         // project key, e.g. "envs/target/labels/old.json"
  kind: string | null;    // the object it belonged to, as MigrateFile names it
  slug: string | null;
}
export interface MigrateResult {
  dryRun: boolean;
  source: string;         // env names as passed
  target: string;
  files: MigrateFile[];   // every file the run considered, in path order
  objects: MigrateObject[]; // per target object, sorted by (kind, slug)
  pruned: MigratePruned[]; // target files removed (or, dry run, to remove)
  prunedDirs: string[];   // target project keys of directories removed
  skipped: string[];      // source keys left out: un-creatable unique email templates
  warnings: string[];     // the text of every warning the run logged
  creates: number;        // objects with change "create"
  updates: number;
  unchanged: number;
  deletes: number;        // objects with change "delete"
  filesChanged: number;   // files whose bytes changed (or would)
  renamed: number;        // files whose path the mapping changed
}
// Rejects with rdc's message on any refusal (recreate guard, dangling overlay
// key, unresolvable saved-view ref, busy env, lock held in another tab, ...).
migrate(src: string, tgt: string, opts: MigrateOptions | null | undefined,
        onLog: (line: string) => void): Promise<MigrateResult>;

export type PromptKind =
  | "conflict" | "remoteDelete" | "bulkConfirm" | "deleteGate" | "deleteDrift"
  | "mdhIndexDrop" | "mdhRowDelete" | "recoverCreate" | "unknown";
```

`MigrateOptions` has no other option: an unknown name throws, as for
`useDataStorageState`. `onLog` gets the same lines as the CLI's `rdc migrate`
output, one call per line, in rdc's colours (as `sync`'s `onLog` does).

## Decisions

### 0. The host does the state's storage I/O (amended 2026-11-12)

*Owner decision.* rdc no longer reads or writes Data Storage for the sync
state. The original browser design says "the host owns persistence"
(`2026-10-26-wasm-browser-design.md`, *Storage-agnostic* and the non-goal
"browser-specific storage code in rdc"); a store that talked to Data
Storage itself broke that, and tied rdc to one storage, one collection
convention and its limits. The host now stores an opaque snapshot per env
through two callbacks, and rdc keeps every decision about it.

- **`load(env)`** — called exactly once at the start of every run of the env:
  each `sync`, each `watch` cycle, and a `migrate` for each of its two envs.
  (Every run reads it, not only a `Project`'s first: that is how a run sees
  what another tab saved since.) Resolve `null` when nothing is stored, else
  `{ rev, snapshot }` as last saved.
- **`save(env, snapshot, expectedRev, nextRev)`** — called at each save point
  (when a run starts, before and after each create, at a run's commit, after
  `_index.md`, after a `migrate`; never under `dryRun`), only when the env's
  files changed since the last load or save. `expectedRev` is the `rev` the
  last `load` returned or the last `save` wrote (`null`: nothing stored yet);
  `nextRev` is `expectedRev + 1` (`1` for the first). Store `snapshot` under
  `nextRev` only if the stored rev is still `expectedRev` (`null`: nothing
  stored), **atomically** — e.g. Data Storage `replace_one` filtered on
  `{_id, rev: expectedRev}`, or `insert` for the first save, where a duplicate
  key means another run won — and resolve `true`; resolve `false` when the
  stored rev is not `expectedRev`. rdc then fails the run with "the state of
  env '<env>' was changed by another run". Resolve `true` only when the
  snapshot is stored; a write whose answer was lost is the host's to settle
  (read back and compare) before it resolves.
- A rejected promise or a throw fails the load or save, and the run, with the
  host's message.
- **`snapshot`** is rdc's opaque format: the gzip of the JSON
  `{files: {path: index}, contents: [text]}`, raw bytes (`state::snapshot`).
  The host encodes it for its storage (base64 for a JSON document). Its size
  limits are the host's: rdc checks none.
- rdc keeps: the save points, `plan_hydrate`'s three-way merge, the per-env
  store cache (one per `Project`, dropped by `useStateStore`, `renameEnv`,
  `removeEnv`), the path checks on load, the "changed by another run" error,
  and the Web Lock.
- Removed: `useDataStorageState` and its `key` / `collection` options,
  `DataStorageStateStore`, `STATE_COLLECTION`, the scoped-store branch of
  `is_internal_collection` (rdc knows no state collection; the `__` and
  `_SA_EXTENSION__` rules stay), the Data Storage client methods only the
  store used (`find`, `replace_where`), the 16 MiB check and the legacy-blob
  handling. The native `StateStore` trait and `FsStateStore` are unchanged,
  and so is the CLI.

Sections §1 to §4 below describe the Data Storage store as built before this
amendment; what they say about what is persisted, and about hydrate, still
holds for the host-backed store, with "the manifest" read as "the stored
snapshot".

### 1. The env tree is part of the Data Storage state

*Owner decision.* The manifest `<key>/<env>` in the env's own org now names
the env's state files **and** every file under `envs/<env>/`:

- `.rdc/state/<env>.lock.json`, `.rdc/state/<env>.base/**`,
  `.rdc/conflicts/<env>/**` (as before);
- `envs/<env>/**` (new): object files, sidecars, `overlay.toml`, `overlay/`,
  `_index.md`, MDH `data.jsonl`, `.<env>-deleted` markers — everything.

Each checkpoint is one compare-and-swap of that manifest, so the lockfile, the
base cache and the env tree in Data Storage are always from the same instant.
That instant is a journal persist (before and after each create) or a commit,
the same points at which the on-disk lockfile is written. So what a closed tab
leaves in Data Storage is exactly what a killed CLI process leaves on disk at
its last lockfile save, minus the files written after it. Crash recovery then
works as on disk.

Two checkpoints are added to `run_cycle` (no-ops on disk, so the CLI is
unchanged):

- **When a run starts** (after hydrate, recovery and the env scaffolds; not
  under `--dry-run`). Without it, a run with no create (an update batch, a
  delete phase) saved nothing before its first remote write: a tab closed
  mid-batch lost the host's edits while half of them were already on the env.
  On disk the edits are in the tree, and the next run finishes them.
- **After `_index.md`** is generated, which happens after the commit. A
  failure of this one is a **warning**, not an error (decided after review):
  the commit's save before it already holds the lockfile, the base cache and
  the tree, `_index.md` is derived from the lockfile and regenerated by the
  next run, and an error would make a host retry (or a watch stop over) a
  sync that is finished on the env. A lost compare-and-swap here is safe for
  the same reason: the next hydrate merges with the commit's manifest as
  its base.

Embedded tests that count checkpoints (`tests/embed_sync_mem.rs`) count these
two too, and the crash-matrix case that crashed at "the first checkpoint"
now names the commit by its lockfile.

Costs, accepted: every checkpoint walks and hashes the env tree. Content must
be UTF-8, as before; a non-UTF-8 file under `envs/<env>/` fails the
checkpoint and names the file.

*Amended 2026-11-12: one document per env.* The per-file blob documents were
far too slow on a real org: one env took about 2,600 `replace_one` requests
on its first save and about 330 `find` requests on every load. That env's
1,318 unique files were 5.2 MB of text, 0.47 MB gzipped. So the store now
keeps `{_id: "<key>/<env>", rev, snapshot}`, where `snapshot` is base64 of
the gzip of `{files: {path: index}, contents: [text]}`: identical contents (a
base copy and its env file) are stored once. A load is one `find` by `_id`;
a save is one `replace_one` filtered on `{_id, rev: N}` writing `rev: N+1`
(the first, one `insert_many`; a duplicate key means another run won). The
CAS, its messages, the path checks, `plan_hydrate`, the store cache, the save
points, the Web Lock and the `key`/`collection` options are unchanged. A
snapshot over Data Storage's 16 MiB limit fails the save and names its size;
nothing is split. With no blobs there is no clean-up and no chunk size:
`chunkBytes` is gone (unreleased, and the extension never passed it).
`flate2` is a shared dependency now (its pure-Rust `miniz_oxide` backend
builds for wasm32). A document in the old blob format, written only by test
runs, reads as no state with a warning, and the next save replaces it at its
`rev`; its blob documents stay behind, unreferenced and harmless.

### 2. Hydrate: which side wins

The store remembers, per env, the manifest it last read or wrote (its
**known** manifest). `Project` keeps one store per env for its lifetime (a
new store when `useDataStorageState` is called again or the env's
`apiBase`/`orgId` changes), so *known* lasts as long as the tab's `Project`.
Let `T` be the tree, `M` the manifest now in Data Storage and `K` the known
one.

- **No `M`:** nothing is written; the first checkpoint publishes the tree.
- **No `K`** (the first run of this env in this `Project` — after a closed
  tab, always): the env's state files **and** `envs/<env>/**` become exactly
  `M`'s: files are written, files `M` does not name are removed. Edits the
  host made to `envs/<env>/` before that first run are therefore replaced.
- **`K` = `M`** (nothing else published since this `Project` last did):
  `envs/<env>/**` is left as it is — the host's and migrate's edits since are
  local changes, as on disk. The state files become `M`'s (as before).
- **`K` ≠ `M`** (another tab or device published since): the state files
  become `M`'s. Each env file is merged three ways with `K` as the base: a
  file only `M` changed takes `M`'s version (or is removed); a file only the
  tree changed keeps the tree's; `_index.md` always takes `M`'s (it is
  generated). A file both changed differently refuses the run, naming the
  files, before anything is written. To take the other run's version, open
  the project again without `envs/<env>/`.

(*Superseded 2026-11-12*: blob ids carried the chunk size after a review
found that mixed chunk sizes on one key corrupted the state; the single
snapshot document of §1 has neither blobs nor chunks.)

So a host loads an env before it edits it: in a new `Project`, the first run
of an env (a `sync` with `noPush` is enough) brings the tree back, and edits
made after it are kept and reach Data Storage when the next run starts.

Why not "Data Storage always wins" for the env tree: the second `sync` of a
session would then erase every edit the host made since the first. Why not
"the tree always wins": after a closed tab the tree is empty, and every
object would read as locally deleted (classify → `LocalDelete`, and the delete
gate offers to delete them on the env) — the bug this fixes.

### 3. What else a closed tab loses

| Path | Read later by | Decision |
|---|---|---|
| `envs/<env>/**` | sync (an absent file is a local delete), migrate (source tree, target tree, `overlay.toml`, `overlay/`) | In the env's manifest (§1). |
| `.rdc/state/<env>.*`, `.rdc/conflicts/<env>/` | sync, recovery, migrate (both lockfiles) | In the env's manifest, as before. |
| `.rdc/mapping.toml`, legacy `.rdc/map/*.toml` | migrate (slug renames); written by sync when it follows a hand rename, and by migrate when it converts legacy files | **Host-owned**, like `rdc.toml`. |
| `rdc.toml`, `secrets/<env>.secrets.json` | every call | Host-owned (unchanged). Credentials never go to Data Storage. |
| `secrets/<env>.hook-secrets.json` | sync's secret pushes | Host-owned. Secret values never go to Data Storage. A lost file pushes nothing: `plan_secret_pushes` walks only the slugs the file names. |
| `.gitlab-ci.yml`, `README.md`, `CLAUDE.md`, testkit, `.gitignore`, `.gitattributes` | nothing a run needs | Not persisted; every wasm `sync` writes them again. |
| `.rdc/state/<env>.lock` (EnvLock) | not used in memory | Not persisted. |

Why the mapping is host-owned: it is one project-level file, but there is no
project-level place in Data Storage — each env's state lives in its own org.
Stored per env, the two orgs would hold two copies with no correct merge: a
row is added by a sync of either env and removed by hand, so neither "newest"
nor "union" is right, and a stale copy makes the next migrate prune the
target object and create a new one. The host already keeps `rdc.toml` and the
secrets, which it needs before rdc can run at all; the mapping is the same
kind of file, and every rdc write to it is reported through `onChange`.

Known limitation, not fixed here: when sync follows a hand rename it records a
mapping row only for envs whose lockfile is in the tree
(`deploy::realign::record_mapping_rows`). In a new `Project` that is only the
envs already synced or migrated in it. Sync every env once before renaming
files by hand.

### 4. A host-configurable state collection

*Owner decision.* `useDataStorageState({ key, collection? })`,
`collection` defaulting to `__rdc_state`. The extension passes
`_SA_EXTENSION__rdc_state`, which does not start with `__`, so the existing
"Rossum's own collection" rule no longer covers it.

`StateStore` gains `fn collection(&self) -> Option<String>` (default `None`);
`DataStorageStateStore` returns its collection.
`cli::pull::mdh::is_internal_collection(name)` is true for a `__` name (as
before), for an `_SA_EXTENSION__` name (see below), **or** for the collection
of the state store scoped to the task. Every MDH path goes through one of
three points:

1. **Listing** (`mdh::list`): the env's collections minus internal ones. Pull,
   push (index and row diffs), the dry-run planner and the stage-4 orphan
   prune all work from this list, so the state collection is never pulled,
   pushed, diffed, or reconciled as a remote delete.
2. **Local-only datasets** (`mdh::local_only_dataset_slugs`, used by the
   create pass and the dry-run planner): a folder whose `collection.json`
   names an internal collection is never created.
3. **Migrate** (new): a source dataset whose `collection.json` names the
   scoped state collection or an `_SA_EXTENSION__` collection is not copied
   to the target.

There is no collection drop anywhere in rdc (`api::data_storage` implements
only `collections/create`), so no path can delete the state collection.

*Owner decision, 2026-11-03:* every collection whose name starts with
`_SA_EXTENSION__` (the extension's prefix) is internal too, exactly like
`__`, **in the CLI as well** — an accepted CLI behaviour change. So the CLI
never pulls, pushes, creates or migrates the extension's state collection
either. Migrate's skip (§4.3) covers `_SA_EXTENSION__*` and the scoped
collection; other `__` datasets in an old tree are still copied, as before.

A project that already pulled such a dataset into `envs/<env>/mdh/`
(verified against the fake, `tests/live/support/fake/extension_collections.rs`):

- The listing no longer shows it, so the next sync's stage-4 orphan prune
  treats it as deleted on the env — **but only when the env lists at least
  one other dataset**. That prune is gated on a non-empty listing, so an MDH
  outage can never mass-delete local datasets. Then an unchanged dataset is
  removed locally (files, base copy, both lockfile entries) and logged as
  `mdh/<slug> (deleted on <env>)`; an edited one is deferred with a
  `-deleted` marker under `.rdc/conflicts/<env>/` (non-interactive) or
  prompted for.
- On an env whose only dataset it is, nothing happens: it stays in the tree
  and the lockfile, inert. Not pushed (only listed datasets are), not
  re-created (`local_only_dataset_slugs` skips it, and a recorded one is left
  to the prune).
- `--allow-deletes` changes none of this: no MDH path deletes a collection
  (there is no drop in rdc) or rows of an unlisted one.
- `rdc migrate` never copies it, and `--mirror` never prunes a target copy
  (the plan would show a delete no sync performs); the target's next sync
  treats that copy as above.

So "it becomes remote-deleted locally on the next sync" holds only when the
env has another dataset. Making it hold always (gating the prune on the
listing before the internal filter) is a separate decision, not taken here.

### 5. `Project.migrate`

*Owner decision.* Runs `cli::migrate::run_at` (through a new
`run_at_logged`, which `run_at` calls with the CLI's log), not a fork.

1. Claims both envs in this `Project` (the in-memory `EnvLock`): a sync,
   watch, rename or remove of either is refused while it runs, and it is
   refused while either is busy.
2. With Data Storage state on: resolves each env's token, holds the target
   org's Web Lock (`rdc:<api_base>:<org_id>`, `ifAvailable`, as `sync`) for
   the whole run, and hydrates the source env and then the target env (§2).
   The source org is not locked: migrate only reads its manifest, which is
   always whole. Without Data Storage state, no Web Lock is taken: nothing
   leaves the tab.
3. Runs migrate on the tree, with the target's store scoped (for §4.3) and
   the log on `onLog`.
4. With Data Storage state on and not a dry run: one checkpoint of the target
   env. A failed checkpoint rejects the call; the tree keeps the migrated
   files.

The result is built from what migrate already computes: each file's
`FileOutcome`, the per-object `ObjStatus` tally, the prune plan, the emptied
directories, the unique-template skips, and the warnings it logs. An object
whose primary file is pruned is reported as `delete`.

**Object identity** (*added 2026-11-04*): a file's `kind`/`slug`, and every
`MigrateObject`, use exactly the identity the sync plan and `skipped` use for
the same object — the lockfile's `(kind, slug)`, with `"mdh"` for a dataset:
a hook's or rule's `.py`/`.js` belongs to its hook or rule, a queue's
`formulas/*.py` to its schema, every leaf of `mdh/<slug>/` to `("mdh",
slug)`, `workflows/<w>/workflow.json` to `("workflows", w)` and
`workflows/<w>/steps/<s>.json` to `("workflow_steps", "<w>/<s>")`, email
templates and engine fields by their compound slugs, `organization.json` to
`("organization", "self")`. So a workflow step is its own `MigrateObject`
(the CLI's summary line keeps counting it with its workflow, unchanged).
A host maps a file it edits directly to the `{kind, slug}` a later sync's
`skipped` names. Each `pruned` entry carries the same identity (*added
2026-11-04*), so a prune maps to the target lockfile entry and to the delete
the next sync plans. `pruned` changed from `string[]` to objects rather than
gaining a parallel array: nothing is released yet, and two arrays matched by
index are easy to misread.

`ObjectChange` describes the **target tree**, not the env (*corrected after
review*: the first draft said "what the next `sync target` does", which the
value never meant): `create` = the tree had no primary file for the object
before this run; `update` = it had one and a file changed; `unchanged`;
`delete` = `mirror` removed its primary file. What the next sync sends also
depends on the target's lockfile and the org: an object deleted locally but
still recorded shows as `create`, and the sync does not POST it again. A
Review screen that needs what will be sent to the env should use the
dry-run sync plan proposed in §7. `--mirror` never prunes a target folder of
a collection rdc skips (§4), on either side.

A `dryRun` saves nothing to Data Storage and writes nothing to the org, but
it still loads both envs (step 2). In a new `Project` that is the envs'
first run, so their `envs/<env>/` becomes the saved tree (files written,
files it lacks removed), exactly as a first `sync` would do it.

A migrate is entirely local until step 4, so a tab closed at **any** point
during its writes leaves Data Storage at the pre-migrate checkpoint; after
reopening, the next run restores the pre-migrate target and the migrate can
be run again with the same result. A tab closed after step 4 leaves the
migrated target tree with the target's unchanged lockfile, which the next
`sync target` pushes.

### 6. A recovery prompt of its own

*Owner decision.* `PromptKind::RecoverCreate`, announced by
`sync::recover::choose`. Its `question` is the whole question — `"<kind>/<slug>:
an unfinished run's create may have made any of N <kind> (ids …). Which one
did it create?"` — and its `choices` are one digit per candidate plus `a`
(abort). Additive: the terminal prompt is unchanged, and the desktop bridge
maps it to its `Conflict` DTO, as before, so the Dart side needs no change.
The wasm `Prompt.question` for every other kind stays the one-line menu.

### 7. A structured dry-run plan, and the items a run left unwritten

*Owner decision, 2026-11-03* (both additive). Before this, `sync(env, {
dryRun: true })` returned all-zero counts: `run_cycle`'s dry-run branch
returned `CycleOutcome::default()` and the plan existed only as log rows.

```ts
export type PlanDirection = "push" | "pull" | "both";   // "both": a conflict
export type PlanChange = "create" | "update" | "delete" | "conflict";
export type PlanPrompt = "deleteGate" | "conflict" | "remoteDelete";
export interface PlanItem {
  kind: string;            // "labels", "hooks", ..., "mdh" for a dataset
  slug: string;
  direction: PlanDirection;
  change: PlanChange;
  prompt: PlanPrompt | null; // what an interactive run would ask about it; without
                           // `interactive`, a deleteGate item refuses the run instead
  note: string | null;     // the log row's note: "new", "secrets", "data", "both diverged", ...
}
export interface PlanRecovery {
  kind: string; slug: string;
  id: number | null;       // the object a crashed run's create made, to be adopted;
                           // null: nothing was made, the file is created anew
}
export interface SyncPlan {
  items: PlanItem[];       // in the log's row order
  recovered: PlanRecovery[];
  problems: string[];      // every line of the dry run's blocks (parse errors,
                           // field limits, missing fields, dangling secrets, ...),
                           // as the log prints them
}
export type SkipReason =
  | "deleteGateDeclined"   // the delete gate was declined (or not answered)
  | "deleteDriftSkipped"   // a delete whose object changed on the env was put off
  | "pushDriftSkipped"     // a push whose object changed on the env was put off
  | "conflictSkipped"      // a both-changed object was skipped (shadow written)
  | "remoteDeleteSkipped"  // a delete on one side was put off (marker written)
  | "mdhIndexDropDeclined" // index drops of a dataset were declined
  | "mdhRowDeleteDeclined";// row deletes of a dataset were declined
export interface SkippedItem { kind: string; slug: string; reason: SkipReason; }
export interface CycleOutcome {
  /* existing counts */
  plan: SyncPlan | null;   // set only by a dry run
  skipped: SkippedItem[];  // what this cycle did not write, and why; empty for a dry run
}
```

- **Plan**, built in the dry-run branch from what it already computes, never a
  second pass: the classified pull, push and prompt items, the MDH plan, the
  secret-only hook pushes, the recovery report, and the bodies of its blocks.
  `prompt` follows the run's own rules: a push delete is `deleteGate` unless
  `allowDeletes`; a both-changed item is `conflict`, and a delete on one side
  with an edit on the other is `remoteDelete`, unless `conflict` (the
  strategy) is set. The MDH planner does not predict index drops or row
  deletes, so MDH items carry no prompt. Recovery is not predicted: a dry run
  already runs it (reads only), and an ambiguous one asks then (a
  `recoverCreate` prompt). The log output is byte-identical; the CLI keeps
  ignoring the field.
- **Skipped**, recorded where each run decides not to write: a task-local
  collector scoped around `run_cycle` (the pattern `stdin_coord::ROUTE` and
  `journal::item` use), with one `record` call at each site. An unanswered
  prompt counts as declined, as it does today. One entry per `(kind, slug,
  reason)`; the delete gate records every object it held back.

Core: `cli::sync::CycleOutcome` gains `plan: Option<SyncPlan>` and `skipped:
Vec<SkippedItem>`; `Default` is `None` / empty, so the CLI and the desktop
bridge are unchanged (the bridge ignores both).

## Tests (TDD)

- Store, against the fake's Data Storage: the env tree round-trips with the
  state; the four hydrate cases of §2, including the both-changed refusal and
  a host edit kept across runs; the collection is the configured one; the
  CAS loser still loses.
- MDH: a listed collection named like the scoped state collection is not
  pulled; a local folder naming it is not created; migrate does not copy it;
  unscoped, nothing changes.
- Closed tab, two fake orgs (`source`, `target`), Data Storage state on:
  1. migrate's checkpoint never happens (any crash during its writes);
     reopen → the target tree is the pre-migrate one, migrate again, sync
     target pushes once;
  2. sync target after migrate dies after a create's POST applied; reopen →
     the migrated tree is back, recovery adopts the object, no duplicate, no
     delete gate, no `LocalDelete`;
  3. a plain reopen after a full sync → tree restored byte-equal, a sync
     changes nothing.
- wasm (`wasm/`, natively with wiremock): option parsing (unknown options,
  `allowRecreate` without `mirror`, `collection`), dry run writes nothing and
  checkpoints nothing, mirror/allowRecreate, only, carry reach `run_at`, the
  result shape, `onLog` gets the lines, the env claims and the Web Lock.
- `recoverCreate` is announced with the full question.
