# Native macOS rewrite of "Rossum Local"

**Date:** 2026-06-29
**Status:** Designed — pending implementation
**Area:** replace `desktop/` (Tauri + React) with `macos/` (SwiftUI) + `rdc-ffi/` (FFI bridge)

## Problem

The desktop app today (`desktop/`, crate `rossum-local`, identifier
`ai.rossum.local`) is a **Tauri** app: ~800 lines of Rust that depend on the
core `rdc` crate (`rdc = { path = ".." }`) plus ~1,070 lines of React 18 +
TypeScript + Tailwind v4 rendered in the system WebView. It is flagged in the
README as *"Work in progress. Not ready for use."* — there is **no released
version**.

We want a **native macOS app** instead of a webview wrapper. The app's job is
small and deliberately thin; its own `commands.rs` header states every operation
*"boils down to writing rdc's standard on-disk artifacts (`rdc.toml`,
`secrets/main.secrets.json`) and then either listing folders or invoking sync.
No registry, no Keychain, no app-private credential store."*

The seven operations and what each really does:

| Tauri command | Real work | rdc core dependency |
|---|---|---|
| `list_connections` | scan `~/Documents/Rossum/*` for folders with `rdc.toml` `[envs.main]` | `rdc::secrets::read_secrets_file` (to derive auth kind) |
| `add_connection` | write `rdc.toml` + secrets | `rdc::slug::slugify_unique`, `rdc::secrets::*` |
| `open_existing_project` | symlink an existing `rdc init` project into the discovery dir | — (fs only) |
| `sync_connection` | scaffold + resolve token + pull | `rdc::cli::init::write_scaffold_files`, `rdc::secrets::resolve_token`, `rdc::cli::sync::embed::sync_no_push` (**pull-only**) |
| `edit_credentials` | rewrite `secrets/main.secrets.json` | `rdc::secrets::*` |
| `remove_connection` | move folder to Trash | `osascript` |
| `reveal_folder` | open in Finder | `open` |

Only two of those genuinely need the Rust core: `resolve_token` (credential
resolution + password→token re-login) and `sync_no_push` (the sync engine, the
heart of rdc). Everything else is trivial Foundation-level filesystem work. So
the central design question is *how a Swift app reaches that Rust core* — and
the answer must not reimplement or fork the sync engine.

## Goal

A native **SwiftUI** macOS app that **preserves today's exact behavior**
(strict parity), **replaces** the Tauri/React app in place, is built
**sandbox-first** for an eventual Mac App Store release, and reaches the heavy
logic through an **in-process FFI bridge (UniFFI)** into the existing `rdc`
crate. The on-disk contract shared with the `rdc` CLI is preserved verbatim so
the CLI and the app stay fully interoperable on the same folders.

## Non-goals (explicitly deferred)

- **No new features.** Pull-only sync (`sync_no_push`), single-env `main`
  projects only, the same seven operations. No push / bidirectional sync, no
  multi-env, no diff preview. These are separate future work.
- **No reimplementation of rdc logic in Swift.** Credential format and the sync
  engine stay in Rust, single source of truth. Swift never parses `rdc.toml` or
  `secrets/main.secrets.json` by hand.
- **No Keychain migration.** Secrets stay in the existing on-disk
  `secrets/main.secrets.json` format (parity + CLI interop).
- **No Developer-ID / notarized non-App-Store build, no App Store submission, in
  this step.** Built sandboxed and run locally with free Xcode signing;
  membership-gated steps come later with no code change (see §8).
- **No Linux/Windows app.** A native macOS app is macOS-only by definition. The
  `rdc` **CLI remains cross-platform**; only the GUI drops the cross-platform
  pretense the Tauri bundle implied.
- **No finer-grained sync progress** than today (started / done / error). The
  current app emits only those phases; matching that is parity.

## Decisions

1. **UI stack:** SwiftUI (with AppKit interop where needed for vibrancy, Dock
   badge, menu bar).
2. **Bridge:** in-process **FFI via UniFFI**. The existing
   `rdc::cli::sync::embed` module is already the intended non-CLI seam (the
   Tauri backend calls it directly); FFI is the minimal generalization of that
   seam for Swift. Chosen over shelling out to the `rdc` CLI because it gives a
   typed result + real progress callback, ships one self-contained `.app` (no
   version-matched `rdc` binary to bundle/locate), and avoids scraping
   human/TTY-formatted CLI output (the CLI has no `--json` progress mode, and
   `--no-push` exists but progress is a non-TTY no-op). The user licensed
   reshaping the Rust as needed, removing the only real downside.
3. **Scope:** strict parity (the seven operations, pull-only, single-env
   `main`).
4. **Transition:** replace `desktop/` in place. It is unreleased, so there is
   nothing to migrate except the on-disk contract, which is preserved.
5. **Distribution posture:** **sandbox-first**, free local Xcode signing now,
   paid membership + Mac App Store submission later with no architectural
   change.
6. **Minimum macOS:** **13.0** (Ventura) — a better SwiftUI baseline than the
   current Tauri floor of 12.0, with a broad install base. Adjustable to 12.0 if
   required.
7. **Identity preserved:** bundle identifier `ai.rossum.local`, product name
   "Rossum Local".

## Architecture

```
┌─────────────────────────────┐
│  SwiftUI app   (macos/)     │  UI · sandbox · security-scoped bookmarks ·
│                             │  Trash/reveal · menu bar · Dock badge · notifs
└───────────────┬─────────────┘
                │ UniFFI-generated Swift bindings (over a C ABI)
┌───────────────▼─────────────┐
│  rdc-ffi crate (staticlib)  │  thin C-ABI surface + dedicated current-thread
│                             │  Tokio runtime for the async/!Send sync engine
└───────────────┬─────────────┘
                │ ordinary Rust calls
┌───────────────▼─────────────┐
│  rdc core      (existing)   │  sync::embed::sync_no_push · secrets::* ·
│                             │  slug::slugify_unique · init::write_scaffold_files
└─────────────────────────────┘
   on disk (shared with the CLI, unchanged):
   rdc.toml · secrets/main.secrets.json · envs/main · .rdc/state/main.lock.json
```

Three layers, each independently understandable:

- **`macos/`** owns everything macOS: the UI, the sandbox/entitlements, the
  security-scoped bookmark store, Trash/reveal, the menu bar, the Dock badge,
  and notifications. It knows nothing about rdc's file formats.
- **`rdc-ffi/`** is a new, small workspace crate exposing a stable C-ABI surface
  via UniFFI. It owns the **async-to-sync adaptation**: a dedicated
  current-thread Tokio runtime on its own thread, on which it `block_on`s the
  `!Send` sync engine — mirroring exactly what the Tauri app does today
  (`spawn_blocking` + `Builder::new_current_thread`). UniFFI catches Rust panics
  at the boundary and converts them to errors; release `panic = "abort"` ensures
  no unwinding crosses the FFI line.
- **`rdc` core** is unchanged in behavior. Reshaping is allowed but confined to
  making the `embed`/`secrets`/`slug`/`init` seams cleanly callable; the sync
  engine itself is not forked.

## FFI surface

Five functions plus DTOs. Trivial OS actions stay in pure Swift.

| FFI function | Replaces | Notes |
|---|---|---|
| `list_connections(parent) -> [ConnectionSummary]` | `list_connections` | reuses today's `discover::scan` logic (relocated from `desktop/src/discover.rs` into `rdc-ffi`) so `rdc.toml` + secrets parsing lives in one place |
| `add_connection(parent, AddConnectionInput) -> ConnectionSummary` | `add_connection` | reuses `rdc::slug::slugify_unique` + `rdc::secrets::*` + writes `rdc.toml` |
| `validate_existing_project(path) -> ConnectionSummary` | `open_existing_project` | validation + summary only; the *attach* (bookmark persistence) is Swift-side, since symlinking is dropped under sandbox |
| `sync_connection(folder, api_base, org_id, progress_cb) -> SyncResult` | `sync_connection` | `write_scaffold_files` + `resolve_token` + `sync_no_push`, run on the dedicated current-thread runtime; coarse phase callback (started/done/error) = parity |
| `edit_credentials(folder, EditCredentialsInput) -> ()` | `edit_credentials` | wipe + rewrite via `rdc::secrets` (same flip-auth-mode behavior as today) |

**Pure Swift (no FFI):**
- `remove_connection` → `FileManager.default.trashItem(at:resultingItemURL:)`
  (native, "Put Back"-recoverable). For an externally-attached (bookmarked)
  project, "remove" drops the bookmark only and **never** trashes the user's
  external folder.
- `reveal_folder` → `NSWorkspace.shared.activateFileViewerSelecting([url])`.

**DTOs** mirror today's structs: `ConnectionSummary` (id, name, api_base,
org_id, folder, auth_kind, last_sync_unix, file_count), `AddConnectionInput`,
`EditCredentialsInput`, `SyncResult` (file_count), a `SyncPhase` callback enum,
and an error enum. UniFFI represents these as records / enums / callback
interfaces.

**Async / `!Send` handling.** `sync_no_push` is `async` and rdc holds `!Send`
types across await points (there is a whole `src/cli/sync/stdin_coord.rs` for
prompt coordination). The FFI exposes `sync_connection` as a **synchronous**
function that internally `block_on`s on the dedicated current-thread runtime;
Swift calls it from a background `Task`/queue and marshals results back to the
main actor. This is the proven pattern already in `desktop/src/commands.rs`.
Cancellation is not in scope for parity (today's app has none) but the runtime
wrapper is structured so a cancel token could be threaded later.

## Sandbox & file-access model

Entitlements: `com.apple.security.app-sandbox`,
`com.apple.security.network.client` (the Rust core makes HTTPS calls to the
Rossum API in-process), `com.apple.security.files.user-selected.read-write`.

The sandbox forbids three things the current app does; each has a clean native
replacement:

| Current (unsandboxed) | Sandbox-native replacement |
|---|---|
| Hardcoded `~/Documents/Rossum/` access | user grants a parent folder once via `NSOpenPanel` → **security-scoped bookmark**; the panel defaults to `~/Documents/Rossum` so CLI interop is unchanged |
| `open_existing_project` **symlinks** an external folder in | bookmark the external folder **in place** — no symlink (a symlink does not grant sandbox access to its target anyway) |
| `osascript` Finder Trash + `open` | `FileManager.trashItem` + `NSWorkspace.activateFileViewerSelecting` |

Because the Rust core does file IO **in the same process**, Swift must hold the
security scope open across the whole operation: it calls
`startAccessingSecurityScopedResource()` before invoking a sync (or any folder
op) and stops afterwards. Sync is long-running, so the scope wraps the entire
`sync_connection` FFI call.

**Bookmark store.** Security-scoped bookmarks must be persisted, which is a
small, sandbox-*mandated* concession to the current "no registry, all state on
disk" philosophy. The store lives in the app container (`UserDefaults` or a
plist) and holds **only access grants** — never connection metadata, which is
still derived entirely from on-disk artifacts. A test-only override hook
(replacing today's `ROSSUM_LOCAL_PARENT` env var) lets tests point at a temp
folder.

## Backward-compatibility contract

**Unchanged and guaranteed:**

- On-disk format: `rdc.toml`, `secrets/main.secrets.json`, `envs/main`,
  `.rdc/state/main.lock.json`. The CLI and the app remain fully interoperable on
  the same folders. Credential reads/writes route through the *same*
  `rdc::secrets` functions — never reimplemented in Swift — so the format cannot
  drift.
- Bundle identifier `ai.rossum.local` and product name "Rossum Local".
- Discovery semantics: a Connection is any folder under the granted parent that
  has `rdc.toml` with an `[envs.main]` section; all displayed state derived from
  disk.

**The only behavioral change:** "open existing project" attaches via a
security-scoped bookmark instead of a symlink — same capability, sandbox-required
mechanism — plus the new app-private bookmark store. Both are called out as
deliberate, sandbox-driven adaptations.

## UI

Faithful SwiftUI port of the current UX, built natively:

- Sidebar connection list + detail pane (the current `Sidebar.tsx` + `Detail.tsx`
  layout).
- Add-Connection and Edit-Credentials sheets (`AddConnectionSheet.tsx`,
  `EditCredentialsSheet.tsx`), token vs. username/password modes.
- Right-click context menu (sync, edit credentials, reveal, remove) and
  remove-confirmation (`ContextMenu.tsx`, `RemoveConfirmSheet.tsx`).
- Empty state (`EmptyState.tsx`).

Native pieces the Tauri app simulates get the real thing: `NSVisualEffectView`
sidebar vibrancy (`.sidebar` material), Dock-icon badge during sync
(`NSApp.dockTile.badgeLabel`), `UNUserNotificationCenter` completion
notifications, a native main menu (`New Connection ⌘N`, `Open Existing ⌘O`,
standard Edit/View/Window menus), and single-instance activation behavior.

## Repo layout & build

- **Remove** `desktop/` entirely (Tauri Rust + React/Vite/Tailwind, including
  `desktop/ui/node_modules`).
- **Add** `rdc-ffi/` — a workspace member with
  `crate-type = ["staticlib", "lib"]` and UniFFI scaffolding.
- **Add** `macos/` — an Xcode project (`RossumLocal`) that links a built
  `.xcframework` (a `lipo` of `aarch64-apple-darwin` + `x86_64-apple-darwin`
  static libs) plus the UniFFI-generated Swift bindings, wired through an Xcode
  build phase (Makefile or `cargo xcframework`).
- Root `Cargo.toml`: change workspace `members` from `[".", "desktop"]` to
  `[".", "rdc-ffi"]`. Honors the existing `edition = "2024"`,
  `dead_code = "deny"` workspace lint, and `panic = "abort"` release profile
  (the last is desirable for FFI).

## Distribution path

- **Now:** free Personal Team, sandboxed, local build & run via Xcode automatic
  signing. No paid membership required to develop or run a sandboxed app.
- **Later (membership-gated, no code change):** add an App Store Distribution
  certificate + App Store Connect record and submit. Because the app is
  sandboxed from day one, nothing in the architecture changes between local
  development and Mac App Store submission. (A Developer-ID/notarized
  direct-download build is also possible later from the same code if
  off-store distribution is ever wanted.)

## Testing

- **Rust:** port the existing `desktop/src/discover.rs` scan tests
  (`scan_finds_rdc_projects_sorts_by_name`, `find_returns_named_connection`,
  `find_returns_none_for_missing`, `auth_kind_is_password_when_username_in_secrets`)
  into `rdc-ffi`; unit-test the FFI DTO mapping and the runtime wrapper.
- **Swift:** unit tests for the bookmark store and the connection-list view
  model; manual verification of each of the seven operations.
- **Interop:** an explicit check that `rdc sync main` (CLI, unsandboxed) and the
  app (sandboxed, same granted folder) produce identical on-disk results —
  proving the backward-compatibility contract.

## Risks / open questions

- **UniFFI async vs. `!Send` sync engine** — mitigated by the dedicated
  current-thread runtime + synchronous FFI called from a Swift background task
  (the pattern already in `desktop/src/commands.rs`).
- **Holding the security scope across a long sync** — wrap the entire
  `sync_connection` FFI call in `start/stopAccessingSecurityScopedResource`.
- **Two spots most wanting sign-off** (both approved in design review): the
  macOS 13.0 floor, and the new app-private bookmark store as a sandbox-mandated
  concession to "no registry".

## Out of scope / future

Push / bidirectional sync; multi-env projects; finer per-file sync progress;
sync cancellation; Keychain-backed secrets; Developer-ID notarized off-store
build; actual Mac App Store submission.
