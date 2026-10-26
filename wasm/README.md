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
env; watch envs; answer the prompts a sync raises.

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
  sync(env: string, opts: SyncOptions, onPrompt: (p: Prompt) => Promise<string | null>,
       onLog: (line: string) => void): Promise<CycleOutcome>;
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
  `WatchHandle.stop()` answers a pending prompt with `null`.
- **One sync or watch per env**: a second call on a busy env is rejected (the
  on-disk `EnvLock` does not apply in memory). `renameEnv` and `removeEnv`
  of a busy env are rejected too.
- **Watch** has no file watcher. It polls on `pollSecs`, and `trigger()` runs a
  cycle as a local file change would; call it after `writeFile`.
