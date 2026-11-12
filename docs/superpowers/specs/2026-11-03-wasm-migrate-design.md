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
export interface DataStorageStateOptions {
  key: string;            // names the project's state; not empty
  collection?: string;    // default "__rdc_state"; not empty
  chunkBytes?: number;    // default 524288
}
// Throws on an empty key or collection, and on any other option name.
useDataStorageState(opts: DataStorageStateOptions): void;

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
}
export interface MigrateObject {
  kind: string;           // "hooks", "queues", "mdh", "workflows", ...
  slug: string;           // the TARGET slug (compound for nested kinds)
  change: ObjectChange;   // what the next `sync target` does with it
}
export interface MigrateResult {
  dryRun: boolean;
  source: string;         // env names as passed
  target: string;
  files: MigrateFile[];   // every file the run considered, in path order
  objects: MigrateObject[]; // per target object, sorted by (kind, slug)
  pruned: string[];       // target project keys removed (or, dry run, to remove)
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
- **After `_index.md`** is generated, which happens after the commit.

Embedded tests that count checkpoints (`tests/embed_sync_mem.rs`) count these
two too, and the crash-matrix case that crashed at "the first checkpoint"
now names the commit by its lockfile.

Costs, accepted: the manifest grows by about 100 bytes per env file, and every
checkpoint walks and hashes the env tree (only changed contents are uploaded;
blobs are addressed by their SHA-256). Content must be UTF-8, as before; a
non-UTF-8 file under `envs/<env>/` fails the checkpoint and names the file.

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

Only blobs of files hydrate writes are downloaded.

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

*Owner decision.* `useDataStorageState({ key, collection?, chunkBytes? })`,
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
- `rdc migrate` never copies it; a `--mirror` prune of a target copy removes
  local files only.

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
