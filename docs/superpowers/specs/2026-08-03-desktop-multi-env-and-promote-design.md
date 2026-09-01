# Desktop app — multiple environments per project + in-app promote

**Status:** Superseded 2026-09-01 — Promote removed; see `docs/superpowers/specs/2026-09-01-desktop-watch-and-promote-removal-design.md`
**Date:** 2026-08-03
**Extends:** `2026-07-24-cross-platform-desktop-flutter-design.md` (which listed
"multiple environments" and "push/deploy" as explicit non-goals — this spec
picks up both).
**Scope:** the Flutter desktop app (`desktop/`) and its bridge crate
(`desktop/rust`, `rdc_bridge`). **The `rdc` CLI core is not modified** — it
already supports everything below; the work is teaching the GUI to use it.

## 1. Problem

The `rdc` CLI core is fully multi-environment: `rdc.toml` holds arbitrarily
named `[envs.<name>]` sections, each with its own `envs/<name>/` snapshot,
`secrets/<name>.secrets.json`, and `.rdc/state/<name>.lock.json`; promotion
between envs exists as `rdc migrate <src> <tgt>` + `rdc sync <tgt>`.

The **desktop app** does not follow. It is hardcoded to a single environment
literally named `main`, and it is pull-only:

- `desktop/rust/src/discover.rs` — `inspect()` only recognizes a folder as a
  project if it has an `[envs.main]` section (`parsed.envs.get("main")?`), reads
  `.rdc/state/main.lock.json`, counts `envs/main`.
- `desktop/rust/src/api/rdc.rs` — `add_connection` writes `[envs.main]`;
  `edit_connection` mutates `envs["main"]`; `validate_existing_project`
  hard-requires the literal string `[envs.main]`; `sync_connection` passes
  `"main"` to scaffold/token/sync. `ConnectionSummary` carries exactly one
  `api_base`/`org_id`/`auth_kind`/`last_sync_unix`/`file_count`.
- `desktop/rust/src/api/rdc.rs::sync_connection` only ever calls
  `rdc::cli::sync::embed::sync_no_push_logged` (pull-only; the embed doc comment
  even reads "the desktop app always uses `main`").

Two consequences:
1. A project with several environments (sand/test/prod) cannot be viewed,
   synced, or promoted from the app.
2. A CLI-created project whose envs are named anything other than `main` (e.g.
   `[envs.dev]`/`[envs.prod]`) is **invisible** to the app — `discover::inspect`
   returns `None` for it.

We want the app to (a) understand the multi-env projects the CLI already
produces, and (b) promote configuration between a project's environments,
in either direction.

## 2. Decisions (fixed during brainstorming)

- **One combined spec** covering both the multi-env foundation and in-app
  promote (user chose combined over phased).
- **UI model = project → env, promote at the project level.** The sidebar is a
  two-level tree: each **project** node expands to its **environment** children.
  Selecting a project shows a **Project view** (environments overview + promote);
  selecting an env shows that env's **Overview / Files / Sync**. Promote never
  appears inside an env subtab — it is a cross-env, project-level concern.
- **Promote is bidirectional** (`sand → test → prod` *or* backport
  `prod → test`), matching the CLI's symmetric N-way mapping.
- **Promote Push = preview + single policy, non-interactive.** After the offline
  Prepare, a dry-run preview lists create/update/delete/conflict against the
  target org; the user picks one conflict policy (use incoming · keep target ·
  skip) and an allow-deletes checkbox; Push runs non-interactively and streams
  the log. No per-conflict GUI dialogs (the CLI's interactive resolver is not
  reimplemented in Flutter).
- **Per-env Sync stays pull-only.** Env views remain read-only mirrors; the
  **only** path that writes to a remote is the gated Promote flow.
- **Terminology:** user-facing "Connections" → **"Projects"**; each project has
  **Environments**; each environment connects to one Rossum **org**.
- **Env names are free-form** (matching the CLI). The new-project flow suggests
  common names (`main`, `sand`, `test`, `prod`) but forces none — in particular
  a brand-new project is **not** forced to name its first env `main`.
- **Prepare mutates local target files** (`migrate` writes `envs/<tgt>/`); the
  remote is untouched until Push. This is surfaced honestly in the UI and
  guarded against clobbering local drift (§9.1). Isolating Prepare in a temp
  tree is deferred (§13).
- **Configure (⚙) is modest in v1** (§10): remembered per-path UI defaults +
  reveal/open of the underlying `.rdc/mapping.toml` / overlay files. No in-GUI
  mapping/overlay editor.

## 3. Goals / non-goals

**Goals**
- View and pull-sync every environment of a project independently.
- Make CLI-created multi-env projects (any env names) visible to the app.
- Promote configuration between two envs of a project, either direction, with a
  reviewable offline preview and a gated, non-interactive push.
- Full backward compatibility with existing single-env `main` projects and with
  the CLI operating on the same folders.

**Non-goals (this iteration)**
- Per-conflict interactive resolution UI (a single policy per push instead).
- Bidirectional per-env sync (pushing an env's local hand-edits to its own
  remote outside promote).
- An in-GUI editor for `.rdc/mapping.toml` or overlays.
- Any change to the CLI core's behavior or on-disk format.
- Temp-tree isolation of the Prepare step (§13, future).

## 4. Verified facts (grounding)

Checked against the code in this repo, not memory:

- **On-disk layout is already per-env** (`src/config/mod.rs`,
  `src/paths.rs`): `rdc.toml` → `envs: BTreeMap<String, EnvConfig>` with
  `api_base` + `org_id`; snapshot at `envs/<env>/`; secrets at
  `secrets/<env>.secrets.json`; lockfile at `.rdc/state/<env>.lock.json`.
- **`migrate` is offline, zero remote calls** (`src/cli/mod.rs` doc + module):
  copies `envs/<src>/` → `envs/<tgt>/`, renames slugs via auto-mapping, rewrites
  `rdc://<kind>/<slug>` refs, applies the target overlay. `--mirror` deletes
  tgt-only objects (local file removals); `--dry-run` prints the plan;
  `--only <selector>` scopes it.
- **`sync` is the push half** (`src/cli/mod.rs`): reconciles a local snapshot
  with the env's remote in dependency order. Flags relevant here:
  `--no-pull` ("Deploy mode: write local edits to the remote but never overwrite
  local files"), `--dry-run`, `--allow-deletes`, and
  `--conflict <use-remote|keep-local|skip>`.
- **`ConflictStrategy`** (`src/cli/resolve.rs`): `UseRemote` = "resolve every
  conflict as `[r] use <env>`" (the remote/target wins); `KeepLocal` = "keep
  local and push it to the env" (local wins); `Skip` = shadow-file fallback
  (leave alone).
- **Embedding entry points** (`src/cli/sync/embed.rs`): `sync_no_push` /
  `sync_no_push_logged(cwd, env, token, sink)` run one pull-only cycle for an
  explicit `cwd`+`env`+`token`, streaming rendered log lines. Analogous embed
  wrappers for push do **not** exist yet and are part of this work (§8).
- **Token resolution** (`src/secrets.rs::resolve_token`, used by
  `sync_connection`) handles silent password re-login; scaffold via
  `rdc::cli::init::write_scaffold_files(folder, env, api_base, org_id)`.
- **The bridge is its own cargo workspace** (parent sets `panic = "abort"`,
  incompatible with FRB unwinding); FRB Dart/Rust glue is committed.

**To verify during implementation (not assumed):**
- That `sync <tgt> --no-pull --dry-run` emits a per-object
  create/update/delete/conflict breakdown suitable for the preview. If the
  existing dry-run output is insufficient, add a thin preview helper in the
  embed layer that returns structured counts (it can reuse the sync classifier).
- The precise `--no-pull` × `--conflict` × `--allow-deletes` interaction on a
  `BothDiverged` object (that keep-local pushes local, use-remote adopts remote
  into local without pulling other objects, skip leaves it) — pin with a live
  or mock-server test before wiring the UI.

## 5. Data model (bridge)

Replace the flat `ConnectionSummary` with a nested project/env shape.

```rust
pub struct ProjectSummary {
    pub name: String,        // folder name (unique within parent, stable)
    pub folder: String,      // absolute path
    pub is_external: bool,   // attached via "Open Existing", outside parent
    pub envs: Vec<EnvSummary>,
}

pub struct EnvSummary {
    pub name: String,        // env key from rdc.toml ("main", "sand", ...)
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind, // Token | Password (per-env secrets file)
    pub last_sync_unix: Option<i64>, // mtime of .rdc/state/<env>.lock.json
    pub file_count: u64,             // files under envs/<env>/
    pub drift: DriftStatus,          // Synced | Drift | NeverSynced (§9.1)
}

pub enum DriftStatus { NeverSynced, Synced, Drift }
```

`envs` is ordered deterministically (env name sort; `main` may be pinned first
for legacy projects — cosmetic). `is_external` moves up to the project because
external-ness is a property of the folder, not an env.

`AppState` (Dart) keys its transient maps (`syncState`, `syncMessage`,
`syncLog`) by **`(folder, env)`** instead of `folder`. Selection state becomes
`{ selectedFolder, selectedEnv? }` where `selectedEnv == null` means the Project
view is showing.

## 6. Discovery (`discover.rs`)

`inspect(folder)` changes from "require `main`" to "enumerate all envs":

- Parse `rdc.toml`; if `envs` is empty or the file is absent/invalid → `None`
  (still not a project).
- For each `(name, cfg)` in `envs`, build an `EnvSummary`: read
  `secrets/<name>.secrets.json` for `auth_kind`, `.rdc/state/<name>.lock.json`
  mtime for `last_sync_unix`, count `envs/<name>/` for `file_count`, compute
  `drift` (§9.1).
- Return `ProjectSummary { name, folder, is_external:false, envs }`.

`scan(parent)` and `find(parent, name)` return `ProjectSummary`. The `Connection`
struct and its `main`-only assumptions are removed.

## 7. UI

### 7.1 Sidebar (two-level)
- Section header renamed **"Projects"**.
- Each project row is a **destination and** an expander: clicking the label
  selects the Project view; clicking the chevron toggles env children.
  (Selecting the project auto-expands.)
- Env children show a per-env status dot (● synced · ◐ drift · ○ never) derived
  from `EnvSummary.drift`.
- A single-env project (e.g. legacy `main`) still shows its one env child; the
  Project view remains available on the parent for consistency.

### 7.2 Project view (parent selected)
- Header: project name + folder + "N environments".
- **Environments** table: one row per env — name, org id, file count, last-sync
  / drift status, and a per-env **Sync** button (pull-only).
- **Promote** panel: `From ⇄ To` env pickers (swap flips direction), a
  **Prepare →** button, a one-line "last promotion" note, and a
  **⚙ Configure mapping & overlays…** affordance (§10). Disabled with a hint
  when the project has fewer than two envs.

### 7.3 Env view (env child selected)
- The existing **Overview / Files** tabs plus a **Sync** action, all scoped to
  the selected env. No promote controls here. Pull-only, unchanged semantics.

### 7.4 Optional pipeline header
The `sand ⇄ test ⇄ prod` topology strip is recorded as a possible later
enhancement to the Project view; the table + promote bar ship first.

## 8. Bridge API surface

Old → new (FRB bindings regenerate; generated Dart/Rust glue is committed):

| Today | This spec |
|---|---|
| `list_connections(parent) -> Vec<ConnectionSummary>` | `list_projects(parent) -> Vec<ProjectSummary>` |
| `validate_existing_project(path) -> ConnectionSummary` | `validate_existing_project(path) -> ProjectSummary` (accepts ≥1 env, any name) |
| `add_connection(parent, AddConnectionInput)` | `add_project(parent, NewProjectInput)` — creates the project + its first env |
| `edit_connection(folder, EditConnectionInput)` | `edit_project(folder, name)` (rename only) + `edit_env(folder, env, EditEnvInput)` |
| `sync_connection(folder, api_base, org_id, sink)` | `sync_env(folder, env, api_base, org_id, sink)` — pull-only, per env |
| `trash_connection(folder)` | `trash_project(folder)` |
| `reveal_in_file_manager(path)` | unchanged |
| — | `add_env(folder, AddEnvInput)` |
| — | `remove_env(folder, env)` — removes the `[envs.<env>]` section, `envs/<env>/`, `secrets/<env>.*`, `.rdc/state/<env>.*`; removing the last env removes the project |
| — | `prepare_promotion(folder, src, tgt, scope, mirror) -> PromotionPreview` |
| — | `push_promotion(folder, src, tgt, ConflictPolicy, allow_deletes, sink) -> stream` |

New/changed inputs and outputs:

```rust
pub struct NewProjectInput { pub name: String, pub first_env: AddEnvInput }
pub struct AddEnvInput {
    pub name: String, pub api_base: String, pub org_id: u64,
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>, pub password: Option<String>,
}
// EditEnvInput mirrors AddEnvInput; blank credentials = keep existing.

pub enum PromoteScope { Everything, Only(Vec<String>) } // Only -> migrate --only
pub enum ConflictPolicy { UseIncoming, KeepTarget, Skip }

pub struct PromotionPreview {
    pub src: String, pub tgt: String,
    pub creates: Vec<ObjChange>,
    pub updates: Vec<ObjChange>,
    pub deletes: Vec<ObjChange>,   // populated only when mirror = true
    pub conflicts: Vec<ObjChange>, // both changed (needs a policy)
}
pub struct ObjChange { pub kind: String, pub slug: String } // e.g. "hooks","validator-invoices"
```

### 8.1 Conflict-polarity mapping (critical — must not invert)

In a promote **push** (`--no-pull`, pushing the migrated *local* `envs/<tgt>`
into the target org), the CLI's "local" side **is** the incoming promoted
config and "remote" is the current target org. The app's user-facing labels
therefore map **inverted** relative to the CLI flag names:

| App label | Meaning | CLI flag | `ConflictStrategy` |
|---|---|---|---|
| **Use incoming** | the promoted config wins | `--conflict keep-local` | `KeepLocal` |
| **Keep target** | the target org's version wins | `--conflict use-remote` | `UseRemote` |
| **Skip** | leave conflicted objects untouched | `--conflict skip` | `Skip` |

This table is the single source of truth for the wiring; a unit test asserts the
mapping so a future refactor can't silently flip it.

## 9. Promote flow (detail)

Entry: Project view → pick `From`/`To` (default `To` = the env you were viewing;
swap flips) → **Prepare**.

1. **Prepare (offline).** Run `migrate <src> <tgt>` with `scope`/`mirror` via an
   embed wrapper (no token). This rewrites local `envs/<tgt>/`. Then run the
   dry-run push preview (`sync <tgt> --no-pull --dry-run`, needs the **target**
   token) and return a `PromotionPreview`. UI copy: "Prepared locally — nothing
   pushed yet."
2. **Review + policy.** Show creates/updates/deletes/conflicts. If any
   conflicts, require a `ConflictPolicy` selection. If any deletes (mirror),
   require the **Allow N deletions** checkbox before Push is enabled.
3. **Push (gated).** `push_promotion` runs
   `sync <tgt> --no-pull --conflict <mapped> [--allow-deletes]` non-interactively
   via a new embed wrapper, streaming the log through the existing `SyncPhase`
   stream. On completion, refresh both envs' summaries.

**Credentials.** Prepare's migrate needs none. The preview and Push need the
**target** env's token; reuse `resolve_token(folder, tgt, api_base)` (silent
password re-login). If it can't be resolved (token-mode, expired, no password),
return a typed "target env needs authentication" error and route the user to the
env's credentials editor rather than failing opaquely.

**Source freshness.** Promote operates on the on-disk `envs/<src>/`. The Project
view shows the source env's drift/last-sync so the user can Sync it first; we do
not force it.

### 9.1 Drift + clobber guard
`drift` for an env = does `envs/<env>/` differ from what was last pulled (its
lockfile/base)? Reuse the sync classifier / the doctor's "local changes not yet
pushed" logic rather than reinventing it.

Because Prepare overwrites `envs/<tgt>/`, if the **target** env has local drift,
Prepare must warn first ("`<tgt>` has N local changes not on its remote;
promoting overwrites them — continue?"). Cancelling leaves the target untouched;
proceeding replaces it. A hint notes that a target can be restored by Sync-ing it
(re-pull) since env sync is pull-only.

## 10. Configure (⚙) — v1 scope
- Remember per promotion-path (src→tgt) UI defaults in app settings: last
  direction, scope (Everything / a saved `--only` selector set), mirror on/off,
  conflict policy.
- Buttons to **reveal**/open `.rdc/mapping.toml` and `envs/<tgt>/overlay/` in the
  OS file manager / editor for advanced edits (auto-mapping already handles slug
  matching; overlays are edited as files).
- No in-GUI mapping/overlay editing — explicitly future work.

## 11. Backward compatibility
- Existing app-made single-env `main` projects load unchanged (one env named
  `main`; Project view + one env child).
- CLI multi-env projects (any env names, no `main`) become visible for the first
  time — a pure gain, no migration.
- `validate_existing_project` accepts any project with ≥1 env; the literal
  `[envs.main]` requirement and its error message are removed.
- On-disk format is untouched; the CLI and app keep operating on the same
  folders. No lockfile/secret/snapshot schema change.
- FRB bindings are regenerated and committed (`flutter_rust_bridge_codegen
  generate`), per the repo's existing process.

## 12. Testing
Follows the existing split (`desktop/rust` cargo tests + `desktop`
`flutter test integration_test`):

**Bridge (offline, cargo):**
- `discover`: multi-env project → N `EnvSummary`; env named `dev` only (no
  `main`) is discovered; empty/invalid `rdc.toml` → not a project; single-env
  `main` still works.
- `add_project` / `add_env` / `edit_env` / `remove_env`: rdc.toml + secrets +
  folders created/updated/removed for the right env; `edit_project` folder
  rename; removing the last env removes the project.
- **Conflict-polarity mapping** unit test (§8.1) — asserts each app policy maps
  to the exact `ConflictStrategy`.

**Bridge against a mock server (integration):**
- `prepare_promotion` on a two-env fixture returns the expected
  creates/updates/deletes(mirror)/conflicts.
- `push_promotion` applies each policy correctly (use-incoming pushes local;
  keep-target adopts remote; skip leaves alone) and honors `allow_deletes`.
- Target-auth-needed path returns the typed error.

**Flutter (widget/integration):**
- Sidebar expands project → envs; selecting project shows Project view,
  selecting env shows env view (no promote controls).
- Promote: Prepare → preview renders; Push disabled until policy/allow-deletes
  satisfied; drift-clobber warning appears when the target has local drift.
- `(folder, env)`-keyed sync state: syncing one env doesn't touch another's log.

## 13. Risks / deferred
- **Prepare mutates the working tree before Push.** Accepted for v1 with the
  drift guard + honest UI messaging. Future: migrate into a temp project copy and only
  commit to `envs/<tgt>/` on Push, making Cancel a true no-op.
- **Single conflict policy per push** is coarser than per-object resolution.
  Acceptable given promote is preview-gated; per-object resolution can be added
  later without changing the data model.
- **Dry-run push preview fidelity** (§4) is the main implementation unknown;
  resolve it first with a mock/live test before building the preview UI.
- **Auth expiry mid-push** surfaces as a streamed error; the app should offer
  re-auth and retry rather than leaving a half-applied push. (The CLI's sync is
  already ordered/idempotent, so a retry after re-auth is safe.)

## 14. Suggested build order (single spec, incremental landing)
1. Data model + `discover` multi-env + `list/validate/sync_env` rename → app
   shows and pull-syncs all envs (foundation; shippable, fixes the invisibility
   bug).
2. Env management (`add_project`/`add_env`/`edit_env`/`remove_env`) + Project
   view table + terminology.
3. Promote: embed wrappers (migrate + dry-run preview + no-pull push),
   `prepare_promotion`/`push_promotion`, the promote panel, policy/allow-deletes
   gating, drift guard.
4. Configure (⚙) defaults + reveal buttons.
