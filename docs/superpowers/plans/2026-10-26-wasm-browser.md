# rdc in the browser (wasm32) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build rdc for `wasm32-unknown-unknown` with a JavaScript API that gives a browser host the desktop app's features over an in-memory project tree, and publish it as a release asset.

**Architecture:** Prompts become async, so no caller has to block. All file access goes through a new `crate::vfs` module that dispatches to a task-scoped store: `std::fs` natively by default, or an in-memory tree. Native-only dependencies and code are gated off on wasm32. A separate `wasm/` crate wraps the embedding surface with `wasm-bindgen`.

**Tech Stack:** Rust (toolchain from `rust-toolchain.toml`), tokio (task-locals, sync primitives), reqwest (fetch backend on wasm), `web-time`, `gloo-timers`, `wasm-bindgen`, `wasm-bindgen-futures`, `js-sys`.

**Spec:** `docs/superpowers/specs/2026-10-26-wasm-browser-design.md`

## Global Constraints

- The native CLI does not change behaviour. Existing test **assertions** stay as they are. Task 1 makes prompt functions async, so tests that call them change only in call syntax (`.await`, `#[tokio::test]`).
- `cargo test --locked`, `cargo clippy --all-targets --locked -- -D warnings` and `cargo doc --no-deps --document-private-items --locked` pass at the repo root after every task, with no wasm toolchain needed.
- On disk, the owner-only secrets permissions and the symlink refusal stay in force. In memory, they do not apply.
- Toolchains come only from `rust-toolchain.toml` (`rustup show`, plus `rustup target add`). Never use `dtolnay/rust-toolchain`. Pin third-party actions by SHA.
- Release asset name: `rdc-<version>-wasm32-unknown-unknown.tar.gz`. Do not change `asset_name` in `src/upgrade.rs`.
- `wasm-bindgen --target web`.
- Never write customer names or identifiers anywhere, commit messages included. Use `acme`, `dev`/`test`/`prod`.
- Gate native-only code with `#[cfg(not(target_arch = "wasm32"))]` at the smallest unit that compiles. Never fork the sync pipeline.

## Review Focus

1. **A spawned task loses the task-local store.** `tokio::spawn` (and `spawn_local` on wasm) does not carry `vfs::scope` or `with_route`. A file write from a spawned task would hit the real disk. Expected: no in-memory run ever touches the real filesystem. Test in Task 4 (`watch_in_memory_never_touches_disk`).
2. **Path forms from the host.** The host passes relative keys (`envs/dev/x.json`). rdc builds absolute paths under the virtual root, sometimes with `.` components. Expected: the same file under every spelling, and a key with `..` that escapes the root is rejected. Test in Task 2 (`mem_normalizes_paths`, `mem_rejects_escape`).
3. **`read_dir` edge cases.** Expected: a missing directory gives `NotFound`, a file gives an error, and entries come in a stable sorted order. Test in Task 2 (`read_dir_contract`, run on both stores).
4. **The host answers a prompt with `null` or a rejected promise.** Expected: the same result as end of input. Conflicts are skipped and gates read as `N`. Test in Task 1 (`route_none_skips_conflict`).
5. **Atomic-write pattern in memory.** The writer writes a temp file and renames it over the target. Expected: rename replaces an existing file, renaming a directory moves its children, and binary (non-UTF-8) bytes survive unchanged. Test in Task 2 (`rename_contract`, `binary_roundtrip`).

---

### Task 1: Async prompts

Make `PromptRoute::ask` async, and move the resolver chain from `BufRead` to an async line source.

**Files:**
- Modify: `src/cli/stdin_coord.rs` (`PromptRoute`, `read_line_coordinated`, `CoordinatorStdin`)
- Modify: `src/cli/resolve.rs` (every `fn …<R: BufRead, W: Write>` at lines 195–1177, and the callers at 1355 and 1483)
- Modify: `src/cli/sync/execute.rs` (`resolve_conflicts`, `resolve_conflicts_projected`, `resolve_one_conflict`, `prune_mdh_orphans`, `resolve_remote_deletes`, `resolve_remote_deletes_projected`)
- Modify: the other `read_line_coordinated` callers: `src/cli/auth.rs`, `src/cli/push/deletes.rs`, `src/cli/push/mdh_data.rs`, `src/cli/push/mdh.rs`, `src/cli/pull/common.rs:968`, `src/cli/sync/embed.rs`
- Create: `src/rt.rs` (the async-runtime shims that every later task uses)
- Modify: `desktop/rust/src/api/rdc.rs:609` (the desktop route)
- Test: unit tests in `src/cli/stdin_coord.rs`. Existing tests in `src/cli/resolve.rs` and `src/cli/sync/execute.rs` get `.await` only.

**Interfaces:**
- Produces, in `src/rt.rs`:
  - `pub type BoxFuture<'a, T>`: `futures::future::BoxFuture` natively, `futures::future::LocalBoxFuture` on wasm32. reqwest futures are `!Send` on wasm, so every `Send` future alias in the crate goes through this one.
  - `pub fn spawn<F: Future<Output = ()> + 'static>(f: F)` (`tokio::spawn` natively, `wasm_bindgen_futures::spawn_local` on wasm; the native one also needs `Send`).
  - `pub async fn sleep(d: Duration)` and `pub type Instant` (tokio's natively, so paused-time tests keep working; `gloo_timers::future::sleep` and `web_time::Instant` on wasm).
- Produces, in `src/cli/stdin_coord.rs`:
  - `pub trait PromptRoute: Send + Sync { fn ask<'a>(&'a self, prompt: &'a Prompt) -> crate::rt::BoxFuture<'a, Option<String>>; }`
  - `pub trait LineSource { fn read_line(&mut self) -> crate::rt::BoxFuture<'_, std::io::Result<Option<String>>>; }`, with `impl<R: BufRead + Send> LineSource for R` (tests keep passing `Cursor`) and `impl LineSource for CoordinatorStdin`. `CoordinatorStdin` no longer implements `BufRead`.
  - `pub async fn read_line_coordinated() -> io::Result<Option<String>>`
- Resolver functions swap `R: BufRead` for `R: LineSource` and become `async`. `W: Write` stays.

- [ ] **Step 1: Write the failing tests** in `src/cli/stdin_coord.rs`:
  - `async_route_answers_through_await`: a route whose `ask` awaits `crate::rt::sleep(Duration::from_millis(1))` and then returns `Some("y")`. `with_route(route, read_line_coordinated())` returns `Ok(Some("y".into()))`.
  - `route_none_skips_conflict`: a route that returns `None`. Drive `resolve_one_conflict` with it, as the existing conflict tests do, and assert the conflict is reported as skipped and nothing is written. This matches today's behaviour for end of input.
- [ ] **Step 2: Run** `cargo test --lib stdin_coord`. Expected: compile failure, because `ask` is not async.
- [ ] **Step 3: Implement** `src/rt.rs` and the signatures above. On the CLI path, the terminal read stays synchronous inside the async function. Its behaviour does not change.
- [ ] **Step 4: Convert the resolver chain and its callers.** Add `.await` to the existing tests. Do not change any assertion.
- [ ] **Step 5: Adapt the desktop route.** Keep the current blocking logic as a private `fn ask_blocking(&self, prompt: &Prompt) -> Option<String>`, which the existing `ask_*` tests call. The trait's `ask` runs it through `tokio::task::spawn_blocking`.
- [ ] **Step 6: Run** `cargo test --locked && cargo clippy --all-targets --locked -- -D warnings` at the root, and `cargo test --locked` in `desktop/rust`. Expected: all pass.
- [ ] **Step 7: Commit** with the message `refactor!: make sync prompts async`.

### Task 2: The `vfs` module

**Files:**
- Create: `src/vfs.rs`
- Modify: `src/lib.rs` (`pub mod vfs;`, `pub mod rt;`)
- Test: unit tests in `src/vfs.rs`

**Interfaces:**
- Produces:
  - `pub trait Store: Send + Sync`, with methods matching `std::fs` semantics and `io::Result` errors (`ErrorKind::NotFound`, `AlreadyExists`, `IsADirectory`/`Other` as std returns them): `read`, `write`, `create_dir_all`, `remove_file`, `remove_dir`, `remove_dir_all`, `rename`, `copy`, `read_dir(&Path) -> io::Result<Vec<DirEntry>>`, `metadata(&Path) -> io::Result<FileType>`, `write_new(&Path, &[u8], mode: Option<u32>)` (create-new, the writer's temp file), `create_dir_all_private(&Path)` (mode `0o700` natively), `is_native(&self) -> bool`.
  - `pub struct DirEntry { pub path: PathBuf, pub file_name: OsString, pub file_type: FileType }` with `path()`, `file_name()` and `file_type() -> io::Result<FileType>`, so existing `entry.…()` call sites compile unchanged. `pub struct FileType` with `is_dir()`, `is_file()`, `is_symlink()`.
  - `pub struct NativeStore;` (wraps `std::fs`, including `symlink_metadata`, so `is_symlink()` stays true on disk) and `pub struct MemStore` (`MemStore::new(root: PathBuf)`, `MemStore::from_files(root, impl IntoIterator<Item = (String, Vec<u8>)>)`, `files(&self) -> BTreeMap<String, Vec<u8>>` keyed relative to the root, `set_on_change(Box<dyn Fn(&str, Option<&[u8]>) + Send + Sync>)`).
  - Free functions mirroring `std::fs` names (`read`, `read_to_string`, `write`, `create_dir_all`, `remove_file`, `remove_dir`, `remove_dir_all`, `rename`, `copy`, `read_dir`, `metadata`, `symlink_metadata`, `write_new`, `create_dir_all_private`), plus `exists(&Path) -> bool`, `is_dir(&Path) -> bool`, `is_file(&Path) -> bool` and `is_native() -> bool`. They dispatch to the scoped store, or to `NativeStore` when none is scoped.
  - `pub async fn scope<F: Future>(store: Arc<dyn Store>, f: F) -> F::Output`, a tokio task-local, the same pattern as `with_route`.
- MemStore rules: directories are implicit or explicit. Paths are normalized lexically (drop `.`, resolve `..`). A path outside the root is an `Err` of kind `PermissionDenied`. `read_dir` is sorted by file name. `symlink_metadata` never reports a symlink. `on_change` fires after each write, rename (once for each side) and delete, with the key relative to the root.

- [ ] **Step 1: Write the failing tests.** Run each contract test against both stores through a helper `fn both(f: impl Fn(&dyn Store, &Path))`. `NativeStore` gets a tempdir and `MemStore` a virtual root `/rdc-mem`.
  - `read_dir_contract`: a missing directory gives `NotFound`, a file path gives `Err`, and entries come back sorted as `["a.json", "b", "c.py"]`.
  - `rename_contract`: renaming over an existing file replaces it. Renaming a directory moves `d/x` to `e/x`.
  - `binary_roundtrip`: `[0xff, 0x00, 0xfe]` reads back the same.
  - `missing_read_is_not_found`: `read` on a missing path gives `ErrorKind::NotFound`.
  - `mem_normalizes_paths`: `/rdc-mem/envs/./dev/x` and `/rdc-mem/envs/dev/x` are the same file.
  - `mem_rejects_escape`: `/rdc-mem/../etc/x` gives `PermissionDenied`.
  - `mem_on_change_reports_relative_keys`: write, then rename, then delete produce `("a", Some)`, `("a", None)`, `("b", Some)`, `("b", None)`.
  - `scope_routes_free_functions`: inside `scope(mem, …)`, `vfs::write` lands in `MemStore::files()`. Outside, it lands on disk.
- [ ] **Step 2: Run** `cargo test --lib vfs`. Expected: compile failure.
- [ ] **Step 3: Implement** `src/vfs.rs`.
- [ ] **Step 4: Run** `cargo test --lib vfs`. Expected: PASS.
- [ ] **Step 5: Commit** with the message `feat: add a scoped virtual filesystem`.

### Task 3: Route all non-test file access through `vfs`

**Files:**
- Modify: every non-test file access in `src/`, about 550 call sites in 50 files. The densest are `cli/push/scan.rs` (99), `cli/deploy/realign.rs` (47), `cli/migrate/mod.rs` (44) and `cli/deploy/selection.rs` (25). `src/upgrade.rs` is excluded: it is native-only and is gated off in Task 5.
- Modify: `src/snapshot/writer.rs:44` (`OpenOptions` with mode → `vfs::write_new`), `src/secrets.rs:58` (`DirBuilder` with mode → `vfs::create_dir_all_private`), `src/paths.rs:109` (`ensure_no_symlinks` → `vfs::symlink_metadata`), `src/cli/sync/lock.rs` (`EnvLock::acquire` returns a no-op guard when `!vfs::is_native()`)
- Create: `tests/vfs_boundary.rs`

**Interfaces:**
- Consumes: Task 2's free functions.
- Rule: `use std::fs;` becomes `use crate::vfs as fs;`, and `std::fs::x(` becomes `crate::vfs::x(`. Each `p.exists()`, `p.is_dir()` and `p.is_file()` on a path becomes `fs::exists(&p)` and so on. Code inside `#[cfg(test)]` stays on `std::fs`.

- [ ] **Step 1: Write the failing test** `tests/vfs_boundary.rs::no_direct_fs_outside_vfs`. It reads every `src/**/*.rs`, cuts each file at its first `#[cfg(test)]` line, and fails, naming the file and line, on any of these regexes: `std::fs\b`, `use std::fs`, `\.exists\(\)`, `\.is_dir\(\)`, `\.is_file\(\)`, `OpenOptions`, `DirBuilder`. The allowlist is `src/vfs.rs`, `src/upgrade.rs`, `src/main.rs` and `src/api/retry.rs`. `retry.rs` is allowed because its HTTP-trace sink appends to a file named by an environment variable and is never active in the browser.
- [ ] **Step 2: Run** `cargo test --test vfs_boundary`. Expected: FAIL, listing about 550 sites.
- [ ] **Step 3: Convert the sites**, one directory per commit if that helps review. Behaviour stays the same, because `NativeStore` is the default.
- [ ] **Step 4: Run** `cargo test --locked && cargo clippy --all-targets --locked -- -D warnings`. Expected: all pass, `vfs_boundary` included.
- [ ] **Step 5: Commit** with the message `refactor: route file access through vfs`.

### Task 4: In-memory embedding (the success criterion)

**Files:**
- Modify: `src/cli/sync/embed.rs`
- Modify: `src/cli/sync/watch.rs` (an external trigger input; every `tokio::spawn` becomes `crate::rt::spawn` and carries the current store and route)
- Create: `tests/embed_sync_mem.rs`, `tests/embed_sync_two_way_mem.rs`

**Interfaces:**
- Consumes: `vfs::{MemStore, scope}`, `rt::spawn`.
- Produces, in `embed.rs` (the existing `sync_logged` and `watch_logged` keep their signatures):
  - `pub async fn sync_logged_in(store: Arc<dyn Store>, root: &Path, env: &str, token: &str, opts: EmbedSyncOptions, log_sink: Box<dyn Write + Send>) -> Result<CycleOutcome>`, which is `vfs::scope(store, sync_logged(…))`.
  - `pub async fn watch_logged_in(store: Arc<dyn Store>, root: &Path, …the watch_logged parameters…, triggers: Option<tokio::sync::mpsc::Receiver<CycleTrigger>>) -> Result<()>`. The host's triggers are merged into the event loop's channel as `CycleTrigger::FileEvent`. This replaces `notify` when the store is not native.
  - `pub use crate::cli::sync::watch::CycleTrigger;`
- Spawning rule: `rt::spawn` wraps the future in `vfs::scope` and `with_route` with the caller's current values, when they are set.

- [ ] **Step 1: Write the failing tests.** Copy `tests/embed_sync.rs` and `tests/embed_sync_two_way.rs` into `*_mem.rs`. In the copies, the project is a `MemStore` at the virtual root `/rdc-mem/project`, seeded with `MemStore::from_files`, and the run goes through `sync_logged_in`. Every file assertion reads `store.files()` instead of the disk. Each test also asserts `!Path::new("/rdc-mem").exists()` (real `std::path`, not vfs). Add `watch_in_memory_never_touches_disk`: a `watch_logged_in` run against wiremock, with one host trigger after a file edit in the store, then cancelled. It asserts the edit was pushed (wiremock received the PATCH) and that `/rdc-mem` does not exist on disk.
- [ ] **Step 2: Run** `cargo test --test embed_sync_mem --test embed_sync_two_way_mem`. Expected: compile failure.
- [ ] **Step 3: Implement** the two functions, the trigger input and the spawning rule.
- [ ] **Step 4: Run** `cargo test --locked`. Expected: all pass, the original embed tests included.
- [ ] **Step 5: Commit** with the message `feat: run sync and watch on an in-memory project`.

### Task 5: The library builds for wasm32

**Files:**
- Modify: `Cargo.toml`. Move `notify`, `fs4`, `inquire`, `tempfile`, `flate2` and `tar`, and tokio's `rt-multi-thread`, `signal` and `io-std` features, under `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]`. Add for wasm32: `web-time`, `gloo-timers` (feature `futures`), `wasm-bindgen-futures`, `getrandom` with its wasm backend feature (check the exact name for getrandom 0.4), and `chrono` feature `wasmbind`. Keep `[dependencies]` versions unchanged.
- Modify: `src/api/mod.rs:121` (`.tcp_nodelay(true)` native only); `src/api/retry.rs`, `src/api/rate_limit.rs`, `src/api/data_storage.rs` (`tokio::time` → `crate::rt`); `std::time::{Instant, SystemTime}` → `web_time::{Instant, SystemTime}` in non-test code (`web-time` re-exports std natively); `src/lib.rs` (`upgrade` native only); and the `cli` items that use `inquire`, `notify`, `fs4`, `std::process` or `tokio::signal`, each gated at the smallest unit.
- Modify: `.github/workflows/ci.yaml`. Add a `wasm` job (ubuntu): `rustup show`, `rustup target add wasm32-unknown-unknown`, `cargo check --lib --locked --target wasm32-unknown-unknown`, plus `cargo clippy` with the same target and `-- -D warnings`. Do not add it to `weekly-release.yaml`: CI may be stricter than the release gate.

- [ ] **Step 1: Run** `cargo check --lib --target wasm32-unknown-unknown`. Expected: FAIL (it fails today on `errno`, a native-only dependency).
- [ ] **Step 2: Gate dependencies and code** until the check passes. Each gate removes a feature only on wasm32.
- [ ] **Step 3: Run** the wasm check above, then `cargo test --locked && cargo clippy --all-targets --locked -- -D warnings` natively. Expected: all pass.
- [ ] **Step 4: Commit** with the message `feat: build the rdc library for wasm32`.

### Task 6: The `wasm/` bindings crate

**Files:**
- Move: the project and env management logic that `desktop/rust/src/api/rdc.rs` keeps today (`add_project`, `validate_existing_project`, `edit_project`, `add_env`, `remove_env`, `rename_env`) into `src/cli/sync/embed.rs` as plain `pub fn`s that take `root: &Path` and go through `vfs`. The desktop bridge then calls them. Its DTOs and FRB types stay in the bridge.
- Create: `wasm/Cargo.toml` (its own workspace, like `desktop/rust`; `crate-type = ["cdylib", "rlib"]`; depends on `rdc = { path = ".." }`, `wasm-bindgen`, `wasm-bindgen-futures`, `js-sys`, `serde-wasm-bindgen`), `wasm/src/lib.rs`, `wasm/README.md`
- Test: `wasm/src/lib.rs` native unit tests, and the desktop bridge tests (unchanged)

**Interfaces:**
- Consumes: Task 4's `*_in` functions, `CycleTrigger`, `MemStore`, and the moved project and env functions.
- Produces, the JavaScript API (the virtual root is internal; the host sees only relative keys):

```ts
export function version(): string;
export class Project {
  constructor(files: Map<string, Uint8Array>);          // an existing project
  static create(name: string, firstEnv: EnvInput): Project; // add_project + scaffold
  files(): Map<string, Uint8Array>;
  onChange(cb: (path: string, bytes: Uint8Array | null) => void): void;
  writeFile(path: string, bytes: Uint8Array): void;     // host-side edits
  deleteFile(path: string): void;
  summary(): ProjectSummary;                            // validate_existing_project
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

  `EnvInput`, `EditProjectInput`, `ProjectSummary`, `RenameEnvResult`, `SyncOptions` (the `EmbedSyncOptions` fields, with the same defaults), `Prompt` (`kind`, `question`, `choices: {key, label}[]`) and `CycleOutcome` are plain objects that mirror the desktop bridge's DTOs field for field, in camelCase. As in the desktop app, `[e]` and `[h]` are filtered out of `choices`. A rejected `onPrompt` promise counts as `null`.
- `Project` holds an `Arc<MemStore>`. Calls that write go through `vfs::scope`. JS callbacks are `!Send`; wrap them for the `Send + Sync` bounds (safe on single-threaded wasm32). Concurrent `sync`/`watch` calls on the same env are rejected with an error, which matches what `EnvLock` did on disk.

- [ ] **Step 1: Write the failing tests** (native, in `wasm/src/lib.rs`, on the Rust side of each binding):
  - `create_then_summary`: `Project::create("acme", dev)` has `rdc.toml`, `.gitlab-ci.yml` and `README.md` in `files()`, and `summary().envs == ["dev"]`.
  - `rename_env_moves_tree`: after `renameEnv("dev", "test")`, no key starts with `envs/dev/` and the files are under `envs/test/`.
  - `on_change_sees_scaffold_writes`: a callback registered before `addEnv` sees every new key.
- [ ] **Step 2: Run** `cargo test` in `wasm/`. Expected: compile failure.
- [ ] **Step 3: Implement** the move in core and the bindings.
- [ ] **Step 4: Run** `cargo test` in `wasm/` and `desktop/rust`, `cargo test --locked` at the root, and `cargo build --release --target wasm32-unknown-unknown` in `wasm/`. Expected: all pass.
- [ ] **Step 5: Commit** with the message `feat: add the wasm bindings crate`.

### Task 7: Release asset, CI and docs

**Files:**
- Modify: `.github/workflows/release.yaml`. Add a `build-wasm` job (ubuntu): `rustup show`, `rustup target add wasm32-unknown-unknown`, `cargo install wasm-bindgen-cli --version <the exact wasm-bindgen version in wasm/Cargo.lock> --locked`, `cargo build --release --target wasm32-unknown-unknown` in `wasm/`, then `wasm-bindgen --target web --out-dir dist/pkg …/rdc_wasm.wasm`, then `tar czf "dist/rdc-$version-wasm32-unknown-unknown.tar.gz" -C dist/pkg .`. Upload it like the CLI artifacts. Add it to the publish job's `needs`, and require `needs.build-wasm.result == 'success'` (my call: blocking, the safer default).
- Modify: `.github/workflows/ci.yaml`. Extend the `wasm` job with the `wasm/` build and the `wasm-bindgen` step, so packaging breaks on a PR and not on release day. Run `cargo test` in `wasm/`.
- Modify: `.github/scripts/release-plan.sh:77`, the ship-gate regex `^(src|templates|desktop|wasm)/`.
- Modify: `tests/release_plan.rs`. Add `wasm_change_ships`, which mirrors the `desktop/` test at line 78: `plan("0.7.0", "wasm/src/lib.rs\n", "fix(wasm): …\n")` releases.
- Modify: `CLAUDE.md` (a bullet: the wasm asset, its name, `--target web`, the `wasm` CI job, why `wasm/` is in the ship gate, and the `vfs` boundary test) and `README.md` (a short "In the browser" section that points to the asset and `wasm/README.md`).

- [ ] **Step 1: Write the failing test** `wasm_change_ships`. Run `cargo test --test release_plan`. Expected: FAIL.
- [ ] **Step 2: Update the regex.** Run the same command. Expected: PASS.
- [ ] **Step 3: Edit the workflows and docs.** Verify locally by running the `build-wasm` commands in a shell. Expected: the tarball contains `rdc_wasm.js`, `rdc_wasm_bg.wasm` and `rdc_wasm.d.ts`.
- [ ] **Step 4: Run** `cargo test --locked`. Expected: all pass. `bumps_all_three_files` is unaffected, because `wasm/Cargo.toml` takes its version from the path dependency and is not bumped.
- [ ] **Step 5: Commit** with the message `ci: publish the wasm build as a release asset`.
