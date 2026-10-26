# rdc in the browser (wasm32) — design

## Intent

Run rdc in a browser, as an alternative to the desktop app. The first
consumer is a Chrome extension (Manifest V3), which lives in another
repository. This repository delivers only the rdc side: a WebAssembly build
of rdc and a JavaScript-callable API on top of it. Nothing outside this repo
is in scope.

Why: an SA should be able to work on a Rossum project from the browser, with
no CLI and no desktop app installed.

## Goals

- **Feature parity with the desktop app.** The browser API covers what
  `desktop/rust/src/api/rdc.rs` covers: create and validate a project; add,
  edit, rename and remove an environment; sync one environment; watch one or
  more environments; answer the prompts that sync raises.
- **Storage-agnostic.** rdc in wasm works on an **in-memory project tree**.
  The host (the extension) gives rdc the tree, rdc runs the operation, and
  the host gets back the changed files. Where the tree is saved — browser
  storage, or a real local folder through Chrome's folder access — is the
  host's decision, not rdc's.
- **Same on-disk format.** The in-memory tree has the same layout as a
  project folder: `rdc.toml`, `secrets/`, `envs/`, `.rdc/state`. A tree
  saved to a folder must work with the CLI and the desktop app unchanged.
- **Secrets are ordinary files in the tree.** `secrets/<env>.secrets.json`
  lives in memory next to the other files. rdc does nothing browser-specific
  for them.
- **Release asset.** Each release publishes
  `rdc-<version>-wasm32-unknown-unknown.tar.gz` next to the four CLI
  archives. It contains the `.wasm` file and the JavaScript glue that
  `wasm-bindgen` generates.

## Non-goals

- Changes to the extension, or to any project repository.
- Browser-specific storage code in rdc (IndexedDB, OPFS, the File System
  Access API). The host owns persistence.
- Encrypting secrets in the browser.
- Promotion between environments (`rdc migrate`, deploy). The desktop app
  leaves this to GitLab CI, and so does the browser.
- The `[e]` (open `$EDITOR`) and `[h]` (hunk-by-hunk) prompt answers. The
  desktop app does not offer them either.
- Desktop-only actions: `trash_project`, `reveal_in_file_manager`.
- `rdc upgrade`, `rdc auth --browser` (the local web form), shell
  completions, and all terminal UI (`inquire`).

## Anti-goals

- The native CLI must not change behaviour. All existing tests keep passing
  with unchanged assertions (the async `ask` forces call-syntax changes —
  `.await` — in tests that drive prompts), and the native safety checks
  stay in force on disk:
  owner-only secrets files (`2b9e393b`) and the symlink refusal
  (`9878cd62`).
- `cargo test` at the repo root must not need a wasm toolchain.
- No forked copy of the sync pipeline for wasm. The browser runs the same
  code as the CLI and the desktop app.

## Success criterion

The embedding sync tests (`tests/embed_sync.rs`, `tests/embed_sync_two_way.rs`)
pass against the in-memory tree, natively, under `cargo test`. CI also checks
that the library builds for `wasm32-unknown-unknown`. A real sync from the
extension is **not** required for this work.

## Constraints and findings

These come from reading the code on 2026-10-26. Nothing has been compiled for
wasm32 yet; the first step is `rustup target add wasm32-unknown-unknown` and a
`cargo check --lib --target wasm32-unknown-unknown` to get the real error list.

- **Filesystem.** Every operation takes `&Paths` (`src/paths.rs`) and calls
  `std::fs` directly: about 13 files outside `src/cli/`, plus most of
  `cli/pull/`, `cli/push/`, `cli/sync/`, `cli/edit/` and `cli/deploy/`.
  This is the largest part of the work.
- **Native-only dependencies.** `notify` (file watching), `fs4` (file locks
  in `cli/sync/lock.rs`, `cli/init.rs`), `inquire` (terminal prompts),
  `tempfile` (atomic writes; `upgrade.rs`), `flate2` and `tar` (`upgrade.rs`
  only), and tokio's `rt-multi-thread` and `signal` features.
- **Timers.** `tokio::time::sleep` and `tokio::time::Instant` do not work on
  `wasm32-unknown-unknown`. They are used in `api/retry.rs`,
  `api/rate_limit.rs` and `api/data_storage.rs`. `std::time::Instant` and
  `SystemTime` (in `log.rs`, `secrets.rs`, `api/retry.rs`, `cli/sync/`)
  panic on that target.
- **Runtime.** `secrets.rs` builds its own tokio current-thread runtime;
  `api/mod.rs` and `cli/sync/watch.rs` call `tokio::spawn`.
- **HTTP.** `build_http_client` (`src/api/mod.rs:121`) calls
  `.tcp_nodelay(true)`, which reqwest's fetch backend does not have.
- **Randomness and clock.** `getrandom` needs its wasm backend; `chrono`
  needs `wasmbind` for `now()`.
- **Process and environment.** `std::process` and `std::env` appear in
  `main.rs`, `upgrade.rs`, `cli/auth.rs`, `cli/resolve.rs`,
  `cli/sync/watch.rs` and others. The embedding path must not depend on
  them; `cli/sync/embed.rs` already avoids `current_dir()` and on-disk token
  lookup for this reason.
- **Existing embedding surface.** `src/cli/sync/embed.rs` (`sync_logged`,
  `watch_logged`, `EmbedSyncOptions`, `PromptRoute`) is what the desktop
  bridge uses. The wasm API should wrap the same surface, and prompts should
  reach JavaScript through a `PromptRoute`, as they reach Dart today (with
  the async `ask` decided below).
- **Release workflow.** `release.yaml` builds CLI archives in a matrix and
  names them with the `tar czf` line. `CLAUDE.md` documents that
  `upgrade.rs`'s `asset_name` must follow that line; the wasm archive is not
  an `rdc upgrade` target, so `asset_name` must not change.
- **CI rules from `CLAUDE.md`.** Toolchains come only from
  `rust-toolchain.toml` via `rustup show` (plus `rustup target add`); never
  `dtolnay/rust-toolchain`. Third-party actions are pinned by SHA
  (`cb5b836e`). The wasm check goes in `ci.yaml`; CI may be stricter than
  the release gate.

## Decided

- In-memory tree as the only wasm storage (user).
- Secrets live in the tree like any other file (user).
- Distribution as a GitHub release asset (user).
- Success = embedding sync tests pass on the in-memory tree (user).
- **`PromptRoute::ask` becomes async** (user). Today it blocks until the
  consumer answers (`src/cli/stdin_coord.rs:115`), and a browser cannot
  block a page or a service worker. With an async `ask`, rdc runs in any
  extension context and needs no cross-origin isolation or
  `SharedArrayBuffer`. All calls go through `stdin_coord.rs`. The CLI's
  terminal behaviour does not change, and the desktop bridge's route
  (a `recv_timeout` loop in `desktop/rust/src/api/rdc.rs`) is adapted to
  the new signature.

## Calls made by the designer (check these)

- **A synchronous storage trait.** rdc code reads and writes through a
  small trait (read, write, list, remove, rename, exists, create dir), with a
  native implementation over `std::fs` and an in-memory implementation.
  Synchronous, because the in-memory tree is synchronous and the host does
  the async loading and saving before and after a call. This avoids
  rewriting the file code as async.
- **Native-only checks become no-ops in memory.** File modes, symlink
  refusal, file locks (`fs4`) and atomic temp-file renames apply only to the
  native implementation. In memory, a write is already atomic and there is
  one caller.
- **Watch in the browser** has no file watcher. It polls the organization on
  a timer, and the host triggers a cycle when the user changes a file.
- **The scaffold is included.** The desktop app writes the project scaffold
  (`write_scaffold_files`, `src/cli/init.rs`: `.gitlab-ci.yml`, `CLAUDE.md`,
  `README.md`, the testkit, the git files) before a sync. The browser writes
  the same files into the in-memory tree. Promotion depends on that
  pipeline, and the templates are embedded strings, so this needs no
  browser-specific code.
- **`wasm-bindgen --target web`.** The glue is an ES module. It loads in a
  page, a dedicated worker, and an MV3 service worker declared with
  `"type": "module"`. The release asset is consumed by another repository,
  so changing the target later is a breaking change.
- **New in-memory tests, existing tests unchanged.** The success criterion
  is met by in-memory copies of `tests/embed_sync.rs` and
  `tests/embed_sync_two_way.rs`, added next to the originals. `sync_logged`
  and `watch_logged` keep their `&Path` signatures for the desktop bridge;
  the in-memory path gets sibling functions that take the storage.
- **The wasm archive is built in the same release run** and is part of the
  published release. Whether a wasm build failure blocks the CLI release (as
  the CLI matrix does) or not (as the desktop jobs do) is left to the
  builder; blocking is the safer default.

## Left to the builder

- Crate layout: `cfg(target_arch = "wasm32")` gates inside the `rdc` crate,
  or a split into a core library crate. Either way, the wasm bindings live in
  their own crate (for example `wasm/`), as `desktop/rust/` does for Dart.
- The exact shape of the JavaScript API, provided it covers the goals above.
- Choice of timer and clock crates for wasm (for example `gloo-timers`,
  `web-time`).
- How the in-memory tree crosses the JS boundary (a map of path to bytes is
  the obvious form).
- Updates to `CLAUDE.md` and `README.md` for the new asset and CI step.
