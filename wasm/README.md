# rdc in the browser (`rdc_wasm`)

JavaScript bindings for rdc on an **in-memory project tree**. The host (for
example a browser extension) hands rdc the project as a map of relative path
to bytes, rdc runs the operation on it, and the host reads the changed files
back. Where the tree is stored is the host's business: rdc has no browser
storage code. The tree has the same layout as a project folder (`rdc.toml`,
`secrets/`, `envs/`, `.rdc/`), so a tree saved to a folder works with the CLI
and the desktop app unchanged.

The API covers what the desktop app does (`desktop/rust/src/api/rdc.rs`):
create and validate a project; edit, add, rename and remove an env; sync one
env; watch envs; answer the prompts a sync raises. It also runs `rdc migrate`
between two envs, which the desktop app leaves to GitLab CI.

## Layout

This crate is **its own cargo workspace** (like `desktop/rust`), so the root
workspace never builds it and `cargo test` at the root needs no wasm
toolchain. Its version is fixed at `0.0.0` and it is never published; the
release names the archive after rdc's version.

- `src/lib.rs` — `ProjectCore`, the plain-Rust logic, with native unit tests.
- `src/bindings.rs` — the `#[wasm_bindgen]` layer (wasm32 only).

## Build

```sh
cd wasm
cargo test                                             # native unit tests
cargo build --release --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir pkg \
  target/wasm32-unknown-unknown/release/rdc_wasm.wasm
```

`wasm-bindgen-cli` must be the exact version `Cargo.toml` pins for
`wasm-bindgen`. The glue is an ES module (`--target web`); it loads in a page,
a worker, and an MV3 service worker declared with `"type": "module"`.

## API

```ts
export function version(): string;
export class Project {
  constructor(files: Map<string, Uint8Array>);              // an existing project
  static create(name: string, firstEnv: EnvInput): Project; // new project + scaffold
  files(): Map<string, Uint8Array>;
  onChange(cb: (path: string, bytes: Uint8Array | null) => void): void;
  writeFile(path: string, bytes: Uint8Array): void;
  deleteFile(path: string): void;
  summary(): ProjectSummary;
  editProject(input: EditProjectInput): ProjectSummary;
  addEnv(input: EnvInput): ProjectSummary;
  removeEnv(env: string): ProjectSummary | null;
  renameEnv(oldName: string, newName: string): RenameEnvResult;
  useStateStore(store: StateStore): void;
  sync(env: string, opts: SyncOptions, onPrompt: (p: Prompt) => Promise<string | null>,
       onLog: (line: string) => void): Promise<CycleOutcome>;
  migrate(src: string, tgt: string, opts: MigrateOptions | null | undefined,
          onLog: (line: string) => void): Promise<MigrateResult>;
  watch(env: string, pollSecs: number | null, onPrompt: (p: Prompt) => Promise<string | null>,
        onLog: (line: string) => void, onIdle: (nextSecs: number | null) => void): WatchHandle;
}
export class WatchHandle { trigger(): void; stop(): void; done(): Promise<void>; }
```

The object types are in the generated `.d.ts`. They mirror the desktop
bridge's DTOs in camelCase. Differences from the desktop app:

- **Keys are relative** to the project root. A key that leaves the tree is
  refused.
- **`onChange`** reports one event per file written, renamed or removed (a
  rename is a removal and a creation). The writer writes the target directly on
  this store, so no temporary file is reported. Replace a key's bytes on a write
  and drop the key on `null`.
- **`ProjectSummary`**: `id` and `name` are the slug of the name given to
  `create` or `editProject` (empty for a project opened from files); `folder`
  is always empty; `lastSyncUnix` is always `null`.
- **`EditProjectInput`** carries the env (`env`), which the desktop passes as
  its own argument.
- **`addEnv`** also writes the env's scaffold, which the desktop app does on
  the first sync.
- **`removeEnv`** of the last env empties the tree and returns `null` (the
  desktop app trashes the folder).
- **Prompts**: `[e]` and `[h]` are never offered. An answer that is not one of
  `choices` — or a rejected promise — counts as `null` (skip / no).
  `WatchHandle.stop()` answers a pending prompt with `null`. `question` is
  the choices on one line, except for `recoverCreate` (several objects on the
  env match a create a closed tab or crashed run left unfinished), whose
  `question` is the whole question; its choices are one digit per candidate
  and `a` (abort the sync).
- **One sync or watch per env**: a second call on a busy env is rejected (the
  on-disk `EnvLock` does not apply in memory). `renameEnv` and `removeEnv`
  of a busy env are rejected too.
- **Syncs of different envs may run concurrently** in one `Project` (each
  holds its own env and, on another org, its own Web Lock). What they share
  is project-level: `.rdc/mapping.toml` (a sync that follows a hand rename
  records a row there, and creates the commented stub if it is missing), the
  scaffold (`.gitignore`, `.gitattributes`, `README.md`, `CLAUDE.md`,
  `.gitlab-ci.yml`, the testkit), and reads of `rdc.toml` and of the other
  envs' lockfiles. No update is lost: every read-modify-write of those files
  is synchronous code with no `await` between the read and the write, and the
  module runs on one thread, so another run cannot interleave inside it
  (`concurrent_syncs_of_two_envs_keep_both_mapping_rows`). The one effect
  across runs: a cycle may count another run's writes to the env API and do
  one extra read-back of its own env (the settle pass), which changes nothing.
- **Web Lock**: `sync` and `watch` hold an exclusive Web Lock for their whole
  run, the browser's `EnvLock`. It is named after the remote org, not the
  project or the state key: `rdc:<api_base>:<org_id>`, with `api_base` as in
  `rdc.toml` without a trailing slash. So every tab that syncs one org
  excludes every other, whether it has a state store or not;
  another org is another lock. It is requested with `ifAvailable`, so a lock
  another tab holds is not waited for: `sync` rejects, and a watch's `done()`
  rejects, with `env '<env>' is already being synced in another tab`. The lock
  is released when the run settles, either way, and by the browser when the
  tab closes or crashes. Where `navigator.locks` is missing, `sync` and
  `watch` reject with `Web Locks are not available in this context` instead
  of running unlocked.
- **Sync state in a host store**: by default an env's sync state
  (`.rdc/state/<env>.lock.json`, `.rdc/state/<env>.base/`,
  `.rdc/conflicts/<env>/`) and its tree (`envs/<env>/`) live only in the
  in-memory tree. After `useStateStore(store)` the next `sync`, `watch` or
  `migrate` also keeps them in the host's storage: one snapshot per env.
  rdc does no storage I/O for it; the host does, through two callbacks:

  ```ts
  interface StoredState { rev: number; snapshot: Uint8Array; }
  interface StateStore {
    load(env: string): Promise<StoredState | null>;
    save(env: string, snapshot: Uint8Array, expectedRev: number | null, nextRev: number): Promise<boolean>;
  }
  ```

  `store` must be an object whose own members are exactly the functions
  `load` and `save` (they are called with `store` as `this`); anything else
  throws. Calling `useStateStore` again replaces the store and forgets what
  this `Project` knew about each env's stored state.
  - **`load(env)`** is called exactly once at the start of every run of the
    env: each `sync`, each `watch` cycle, and a `migrate` once for each of its
    two envs. Resolve `null` when nothing is stored for `env`, else the `rev`
    and `snapshot` last saved.
  - **`save(env, snapshot, expectedRev, nextRev)`** is called at each save
    point (below), only when the env's files changed since the last load or
    save. `expectedRev` is the `rev` the last `load` returned or the last
    `save` wrote, `null` when nothing is stored yet; `nextRev` is
    `expectedRev + 1` (`1` for the first). Store `snapshot` under `nextRev`
    only if the stored rev is still `expectedRev` — **atomically** (with Data
    Storage: `replace_one` filtered on `{_id, rev: expectedRev}`, and for the
    first save an `insert`, where a duplicate key means another run won) —
    and resolve `true`. Resolve `false` when the stored rev is not
    `expectedRev`: rdc fails the run with `the state of env '<env>' was
    changed by another run`. Resolve `true` only when `snapshot` is stored;
    a write whose answer was lost is the host's to settle (read back and
    compare) before it resolves.
  - A throw or a rejected promise fails the load or save, and the run, with
    the host's message.
  - **`snapshot`** is rdc's opaque format (the gzip of a JSON map of the
    env's files, identical contents stored once); store the bytes as they
    are (base64 them for a JSON document). Its size limits are the host's
    storage's: rdc checks none. One env measured at 5.2 MB of text was
    0.47 MB gzipped. Every file must be UTF-8.
  - **When it is saved**: when a run starts (not under `dryRun`), after every
    lockfile write of a sync (before and after each create, and at its end),
    after `_index.md` (a failure there is a warning: the save before it
    already holds the state and the tree), and after a `migrate` (not under
    `dryRun`). Each save is one compare-and-swap, so the stored state and
    tree are always from the same moment.
  - **When it is loaded**: at the start of every run of the env. The state
    files are always the saved ones. For `envs/<env>/`: the first run of an
    env in a `Project` takes the saved tree whole (files are written, files
    it does not name are removed), so after a closed tab the tree comes back
    with its state and nothing reads as a local delete. Later runs of the
    same `Project` keep the tree's own changes (the host's edits, migrate's
    output), and take what another tab or device saved since for files this
    tab did not change; a file both changed refuses the run and names the
    files. Edits written into `envs/<env>/` before an env's first run in a
    `Project` are therefore replaced: load the env first (a `sync` with
    `noPush`), then edit. An edit reaches the host's store when the next
    run starts.
  - **What it does not keep**: `rdc.toml`, `secrets/`, `.rdc/mapping.toml`
    (and legacy `.rdc/map/`) are the host's to keep, as before; rdc reports
    every write to them through `onChange`. The scaffold (`.gitlab-ci.yml`,
    `README.md`, `CLAUDE.md`, the testkit) is written again by every `sync`.
    See `docs/superpowers/specs/2026-11-03-wasm-migrate-design.md` for why.
- **Migrate**: `migrate(src, tgt, opts, onLog)` runs `rdc migrate` on the tree
  (`cli::migrate::run_at`). `opts` — `dryRun`, `mirror`, `allowRecreate`
  (needs `mirror`), `only` (selectors) and `carry` (`"scoreThresholds"`,
  `"emailPrefixes"`, `"automation"`, `"all"`) — are the CLI's flags; any other
  option throws. It claims both envs, as a sync claims one. With a state
  store set, it loads both envs' state first, holds the target org's Web Lock,
  and saves the target after (not under `dryRun`). `onLog` gets the CLI's
  output. The result is the plan: every file considered (`files`, with
  `source` and `target` keys, `"create"` / `"update"` / `"unchanged"`, and
  the `kind` and `slug` of the target object it belongs to — the same
  identity a `sync`'s `plan` and `skipped` use, so a file the host edits maps
  to what a later sync reports; sidecars (`.py`, `.js`, `formulas/*.py`)
  name their hook, rule or schema, every file of `mdh/<slug>/` names
  `mdh`/`<slug>`, a workflow step `workflow_steps`/`<workflow>/<step>`;
  `null` for a file of no object),
  every target object (`objects`, by target slug, with what this run did to
  its files in the target tree: `"create"` = the tree had no file for it,
  `"update"` = its files changed, `"unchanged"`, `"delete"` = `mirror`
  removed it). This is the tree, not the env: what the next `sync` sends
  also depends on the target's lockfile and the org (an object deleted
  locally but still on the env shows as `"create"`, and the sync does not
  create it again),
  the `pruned` files (each `{ target, kind, slug }`, the same identity as a
  file's) and `prunedDirs`, the `skipped` email templates, the
  `warnings` it logged, and their counts. `mirror` never prunes a folder of
  a collection rdc skips (`__*`, `_SA_EXTENSION__*`).
  A `dryRun` saves nothing, but its loading of the two envs is a run like
  any other: in a new `Project` it is the envs' first run, so `envs/<env>/`
  becomes the saved tree there, as described above. A refusal (recreate guard, a
  dangling overlay key, an unresolvable saved-view ref) rejects with rdc's
  message. Then `sync(tgt, ...)` pushes it.
- **`CycleOutcome.plan`**: a `sync` with `dryRun` returns what it would do as
  data, besides the log: `items` (one per object, in the log's order:
  `kind`, `slug`, `direction` `"push"` / `"pull"` / `"both"` for a conflict,
  `change` `"create"` / `"update"` / `"delete"` / `"conflict"`, `prompt` —
  `"deleteGate"`, `"conflict"` or `"remoteDelete"` when an interactive run
  would ask, `null` when `allowDeletes` or `conflict` waives it — and the log
  row's `note`; MDH datasets have kind `"mdh"` and no prompt), `recovered`
  (creates a crashed run left, which the dry run's recovery found: `id` to
  adopt, or `null`), and `problems` (every line of the dry run's blocks:
  parse errors, field limits, ...). Any other run returns `plan: null`.
  Recovery itself is not predicted: a dry run runs it, and an ambiguous one
  asks then (`recoverCreate`).
- **`CycleOutcome.skipped`**: what the cycle did not write, and why: one
  `{ kind, slug, reason }` per object, `reason` one of
  `"deleteGateDeclined"`, `"deleteDriftSkipped"`, `"pushDriftSkipped"`,
  `"conflictSkipped"`, `"remoteDeleteSkipped"`, `"mdhIndexDropDeclined"`,
  `"mdhRowDeleteDeclined"`. An unanswered prompt counts as declined. Empty
  for a dry run.
- **Watch** has no file watcher. It polls on `pollSecs`, and `trigger()` runs a
  cycle as a local file change would; call it after `writeFile`.
