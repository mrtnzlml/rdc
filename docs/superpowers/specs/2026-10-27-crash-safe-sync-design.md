# Crash-safe sync: create journal, recovery, pluggable state store

## Goal

A sync that is interrupted at any point — process killed, browser tab closed,
network gone — must never leave a duplicate remote object behind, and the next
run (on this machine or another) must finish the job without a human. The same
must hold when rdc runs in the browser and keeps its `.rdc/` state in Rossum
Data Storage instead of on disk. Existing CLI behaviour stays unchanged except
where this document says otherwise.

## What is already safe (verified in code, to be pinned by tests)

Only one kind of remote write is not idempotent today. Everything else
converges on a re-run:

| Write | Why a re-run is already safe | Source |
|---|---|---|
| PATCH (all kinds) | A re-run sees local == remote, both != stale base, and classifies `RemoteEdit` (convergence), never a second PATCH or a prompt. | `src/cli/sync/classify.rs:78` |
| DELETE | `delete_path` treats 404 as success; the tombstone then resolves as `BothDeleted`. | `src/cli/push/deletes.rs` (point 3) |
| MDH rows | Keyed diff: explicit `_id` rows use `replace_one`; id-less rows are a content multiset, so rows already inserted are not inserted again. | `src/cli/push/mdh_data.rs` |
| Store-extension hook install | The two-call install already adopts a partial install. | `src/cli/deploy/store_extensions.rs` |
| Pull | Re-reads the remote. | — |

**POST creates are not safe.** `src/cli/push/labels.rs` (and every other create
driver) does POST → in-memory `lockfile.upsert` → disk + base cache write. The
lockfile reaches disk only at the end of the cycle or on the error path
(`src/cli/sync/mod.rs:801`, `:823`). If the run dies after the server applied
the POST and before that save, the id is lost. The next run then either POSTs
again (local slug ≠ `slugify(remote name)`) or classifies the pair as
`BothDiverged` and prompts (`classify.rs`, the "lockfile entry missing" arms).

A second, existing gap: `src/api/retry.rs` retries 502/503/504 for every
method, POST included. A gateway error after the backend applied a create
duplicates it within one run.

## Design

### 1. Lockfile v4 with a `pending` journal

`LOCKFILE_VERSION` becomes 4. v1–v3 load and migrate as today (v3 → v4 only
sets the version; `pending` absent). A lockfile above 4 is refused with the
existing "Run `rdc upgrade`" message. The bump is deliberate: v3 rdc ignores
unknown fields (no `deny_unknown_fields`), so it would load a v4 file and its
next save would silently erase the journal.

```json
"pending": {
  "run": "<32 hex chars>",
  "ops": [
    {"seq": 1, "kind": "labels", "slug": "audit-hold",
     "fields": {"name": "<sha256>", "queues": "<sha256>"},
     "started_at_unix": 1792999400, "id": null}
  ]
}
```

- `pending` is omitted when empty, so a run that creates nothing writes a
  byte-identical lockfile to today's (apart from `"version": 4`).
- `fields` holds a SHA-256 of each top-level field of the POST body (canonical
  JSON), never the value — the lockfile is committed and a body can carry hook
  code or secrets. `secrets` is never hashed.
- `id` is filled in when the POST response arrives.

### 2. Journal: only creates are journaled

`src/state/journal.rs` owns a `Journal` handle shared by the run (an
`Arc<futures::lock::Mutex<…>>`, so concurrent drivers serialize their writes;
creates are sequential today anyway, see `push/concurrent.rs`). It keeps the
**last committed lockfile** plus `pending`. Every journal write persists
"committed objects + current pending" — a write-ahead log over the last
checkpoint. Drivers' `&mut Lockfile` is untouched.

- `begin` (before the POST): append an op, persist.
- `finish` (after the response): set `id`, persist.
- `commit` replaces the two `lockfile.save` calls in `sync/mod.rs`: it saves
  the driver's lockfile with every op whose `(kind, slug)` entry now has that
  `id` removed, keeping the rest. When no op is left, `pending` disappears.

The `(kind, slug)` of a call comes from a task-local set by the driver around
each create (`journal::item(kind, slug, fut)`), the pattern
`stdin_coord::ROUTE` and `vfs::scope` already use. A create with no item in
scope (unit tests calling the client directly) is not journaled.

Updates, deletes and pulls are not journaled: the table above shows they
converge. This keeps two saves per create as the only new disk traffic.

### 3. Mutual exclusion (no lease)

*Decided 2026-10-27, replacing a 10-minute lease.* A run that finds any
`pending` recovers it at once (§4). What makes that safe is that the writer of
that `pending` cannot still be running:

- **CLI and desktop:** the existing `EnvLock` (an OS lock on
  `.rdc/state/<env>.lock`, released by the OS when the process dies) is held for
  the whole run, so a second run on the machine waits for the first.
- **Browser:** the wasm `Project` holds a Web Lock (`navigator.locks`,
  exclusive, named `rdc:<api_base>:<org_id>`) for the whole sync or watch.
  The browser releases it when the tab closes or crashes, so it plays the
  `EnvLock` role across the extension's tabs.

**Accepted edge case:** two machines, or two browsers, running at the same
moment against one env and one shared state can still race: B recovers A's
in-flight create, finds no object yet and POSTs; A's POST then lands → a
duplicate. The Data Storage manifest's compare-and-swap (§6) makes A's next
save fail loudly, so the duplicate is visible, not silent. `pending.run` stays
in the format for messages and diagnostics; there is no expiry field.

### 4. Recovery

`src/cli/sync/recover.rs` runs after `Lockfile::load` and before classify:

- **Op with `id`:** GET the object. 404 → drop the op (the object is gone; the
  local file becomes a normal `LocalCreate`). Found → complete the create
  exactly as the driver would have: record the lockfile entry, write the
  canonical bytes to the env tree and the base cache (the post-create path in
  each driver), drop the op.
- **Op without `id`** (died while the POST was in flight): list the kind and
  take remote objects that no lockfile entry claims and whose
  `IDENTITY_FIELDS[kind]` hashes match. With several, narrow by all `fields`. For hook `config` and engine
  `settings` the server fills in defaults, so they are hashed and matched per
  sent sub-key (extra server sub-keys ignored; hook `config.code` is hashed
  trimmed, as the server drops trailing whitespace).
  - 0 → drop the op; the normal `LocalCreate` POSTs it.
  - 1 → complete as above.
  - more → interactive: prompt with one choice per candidate id plus `[a]bort`;
    `--yes` / non-TTY: stop with an error naming the candidate ids.

Recovery is deterministic, so two machines recovering the same op record the
same id.


*Amended 2026-11-13: which local file recovery overwrites.* As built,
completing never overwrote a local env file: it recorded the entry and the
base and left the file, so the classifier saw the file as `Clean` or as a
`LocalEdit`. That diverged from a normal run, which rewrites the file with
the server's object. A file written without a field the server fills on
create — a hand-written label or workspace with no `organization` (`rdc
migrate` keeps whatever `organization` the source file had, so a migrated
copy of such a file lacks it too) — then read as a `LocalEdit`, and the push
failed deserializing it (`missing field 'organization'`). Found live, with the
state in a host store.

Decision: a local file that still builds exactly the body the crashed run
sent — read, refs resolved, `strip_for_create`, and its per-field hashes
(`journal::create_fields`) equal to the op's `fields` — is completed exactly
as the driver completes it, file rewritten canonically. Any other file is
kept, as before: one edited after the crash is the user's, and the next push
sends the edit. The check applies to the kinds whose driver body is the file
and nothing more (`labels`, `workspaces`, `inboxes`, `saved_views`,
`engine_fields`); the others build more than the file (hook code, schema
formulas and queue-scoped completion, engine deferred refs, queue and rule
preparation), so for them the file is kept as before. Affected kinds — a
model field the server fills on create that a file can lack — are `labels`
and `workspaces` (`organization`); the crash matrix pins both
(`label_without_organization_after_apply`,
`workspace_without_organization_after_apply`). An inbox without `queues` was
checked too and is not affected. Not chosen: making the PATCH path tolerate
missing server-filled fields, which would leave recovery's tree different
from a normal run's and only move the divergence.

### 5. Create retries

For a journaled create, a 502/503/504 is not resent blindly: the client runs
the §4 matcher first. 1 match → that object is the result; 0 → resend (as
today); more → error naming the ids. 429 and connect-class failures keep
today's retry (the server did not apply the request).

### 6. Pluggable state store

`src/state/store.rs`:

```rust
pub trait StateStore: Send + Sync {
    /// Bring this env's state into the vfs before the run reads it.
    fn hydrate<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>>;
    /// Make the vfs state durable. Called after every journal persist and commit.
    fn checkpoint<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>>;
}
```

The state is `.rdc/state/<env>.lock.json`, `.rdc/state/<env>.base/**` and
`.rdc/conflicts/<env>/**`. `.rdc/mapping.toml` is hand-written and stays in
the project tree.

*Amended 2026-11-03* (`2026-11-03-wasm-migrate-design.md`): the Data Storage
manifest also holds `envs/<env>/**`, its collection is chosen by the host,
`run_cycle` checkpoints when a run starts and after `_index.md` too, and a
hydrate decides per file which side wins. *Amended 2026-11-12*: the blob
documents and the per-file manifest below were replaced by one gzipped snapshot
per env, and then rdc stopped doing that storage I/O at all: the host stores
the snapshot through `useStateStore` (that spec's §0). The text below describes the
state-only store as first built.

- **`FsStateStore`** (CLI, desktop, the wasm default): both methods are
  no-ops; files are already written atomically on disk. The CLI's behaviour is
  therefore unchanged; `EnvLock` stays.
- **`DataStorageStateStore`** (browser): one collection (`__rdc_state`; the `__` prefix keeps it out of MDH sync).
  - Blob documents `{_id: "<key>/<env>/blob/<sha256>/<n>", text}`, content
    split into chunks of at most `chunk_bytes`. Content must be UTF-8.
  - One manifest `{_id: "<key>/<env>", rev, files: {"<relpath>": {"sha256", "chunks"}}}`.
  - `hydrate`: read the manifest and its blobs, write them into the vfs; it refuses a manifest naming a path outside this env's state files.
  - `checkpoint`: upload the missing blobs (`replace_one` upsert by `_id`,
    idempotent), then `replace_one` the manifest filtered on
    `{_id, rev: N}` with `rev: N+1`. A filter that matches nothing → error
    "the state of env '<env>' was changed by another run"; the run stops. The
    first manifest is created with `insert_many` of one document; a duplicate
    key means another run won. After a successful manifest write, delete only the
    blobs the previous manifest referenced and the new one does not (`known − next`).
  - `key` is supplied by the host (the extension knows its org/project).

The wasm `Project` gets `useDataStorageState(options)`; without it the
in-memory behaviour is today's.

## Behaviour changes (the complete list)

1. The lockfile says `"version": 4`; v3 rdc refuses it ("Run `rdc upgrade`").
2. During a create, the lockfile on disk carries `pending` (two extra saves per
   create).
3. A run that finds a `pending` recovers it first; with several candidates it
   prompts (interactive) or stops (`--yes`).
4. In the browser, `sync`/`watch` hold a Web Lock for the env, so a second tab
   waits (or is refused) instead of running alongside.
5. A 502/503/504 on a create runs the matcher before resending.
6. New wasm API `useDataStorageState`.
7. The lockfile never carries a create-body value; hook `config` and engine `settings` are journaled per sent sub-key.

Everything else — the classification, prompts, pull output, the transcript,
the byte content of env files and base cache — must be identical, pinned by the
existing suite passing unchanged except for version literals.

## Open facts (settled against the test org)

- `replace_one` reports `matched_count`, `modified_count`, `upserted_id`; a stale `{_id, rev}` filter matches 0 and writes nothing, so the CAS works.
- Document size limit is MongoDB's 16 MiB; `chunk_bytes` stays 524288.
- Collection `__rdc_state` is created implicitly by the first write and is listed like any other; only the MDH mirror hides it.
- The session token can write Data Storage.
- Dotted manifest keys (`.rdc/state/<env>.lock.json`) are accepted as field names and read back byte-equal.
- A duplicate `_id` on `insert_many` answers 400 and leaves the existing document unchanged.
- Server rewrites on create: hook `config` gains defaults and its `code` loses trailing whitespace; engine `settings: {}` becomes `{"use_case": ...}`; every other kind echoes sent fields unchanged. Store-extension and webhook hooks were not covered.
