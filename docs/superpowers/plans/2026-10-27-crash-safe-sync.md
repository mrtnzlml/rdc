# Crash-safe Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An interrupted sync never leaves a duplicate remote object, and the next run on any machine finishes it — on disk and with state kept in Rossum Data Storage from the browser.

**Architecture:** Lockfile v4 gains a `pending` write-ahead journal of creates plus a lease. A shared `Journal` persists it around every POST; a recovery step completes or drops pending creates before classify. A `StateStore` trait makes `.rdc/` state durable: a no-op on disk, Data Storage blobs + a CAS manifest in the browser.

**Tech Stack:** Rust (edition per `Cargo.toml`), tokio task-locals, `futures::lock::Mutex`, `sha2`, wiremock (fake org), wasm-bindgen.

**Spec:** `docs/superpowers/specs/2026-10-27-crash-safe-sync-design.md`

## Global Constraints

- Work on branch `feat/crash-safe-sync`, never on `main`.
- Gate after every task: `cargo test --locked`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo doc --no-deps --document-private-items --locked`, `cargo check --lib --locked --target wasm32-unknown-unknown`, and in `wasm/`: `cargo test --locked`.
- Existing tests must pass **unchanged**, except literals `3` → `4` for the lockfile version. Any other edit to an existing assertion needs the user's approval first.
- All file access goes through `crate::vfs` (`tests/vfs_boundary.rs`).
- Lease TTL: exactly 600 seconds. Data Storage collection default: `rdc_state`. Initial `chunk_bytes`: 524288.
- The lockfile never contains a create-body value — only SHA-256 hashes; the `secrets` field is never hashed.
- No customer names or customer identifiers anywhere, including commit messages (see `CLAUDE.md`). Test org data uses the live suite's `rdc-it-<id>-` prefix.

## Review Focus

1. A lockfile with `pending` committed to git and pulled on a machine that lacks the env file → recovery completes the create and writes the file (Task 5 test `recovers_on_a_machine_without_the_local_file`).
2. Two browser tabs syncing one env against Data Storage → the second checkpoint fails loudly, nothing is overwritten (Task 7 `second_writer_loses_the_cas`).
3. A hook create with secrets → no secret value and no `secrets` hash ever appears in the lockfile (Task 2 `secrets_never_hashed`, Task 6 `hook_secret_never_reaches_lockfile`).
4. A POST rejected with 4xx → its op is removed at once, not left to block the next run (Task 4 `rejected_create_leaves_no_pending`).
5. A `pending` whose expiry is far in the future (bad clock) → refusal names the run and expiry time; it does not wait (Task 5 `unexpired_foreign_lease_refuses_even_with_yes`).

---

### Task 1: Lockfile v4 with `pending`

**Files:**
- Modify: `src/state/lockfile.rs` (version const, struct, `load`)
- Modify: tests that assert the literal version (find with `grep -rn 'LOCKFILE_VERSION\|"version": 3' src tests`)

**Interfaces:**
- Produces: `pub const LOCKFILE_VERSION: u32 = 4;`
- Produces in `src/state/journal.rs` (types only in this task): `pub struct Pending { pub run: String, pub expires_at_unix: u64, pub ops: Vec<CreateIntent> }`, `pub struct CreateIntent { pub seq: u64, pub kind: String, pub slug: String, pub fields: BTreeMap<String, String>, pub started_at_unix: u64, pub id: Option<u64> }` — both `Serialize, Deserialize, Debug, Clone, PartialEq`.
- Produces: `Lockfile.pending: Option<Pending>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`.

- [ ] **Step 1: Write failing tests** in `lockfile.rs` tests: `v3_migrates_to_v4_without_pending` (load `{"version":3,…}` → `version == 4`, `pending == None`); `v5_is_refused_with_upgrade_hint` (error contains `Run \`rdc upgrade\``); `pending_round_trips` (save+load equal); `no_pending_serializes_without_the_key` (saved text lacks `"pending"`).
- [ ] **Step 2: Run** `cargo test --lib state::lockfile` — expect the four to FAIL.
- [ ] **Step 3: Implement**: bump the const, add the field, extend `load`'s match with `1 | 2 | 3` → set version.
- [ ] **Step 4: Update version literals** in existing tests (only `3` → `4`), run the full gate. Expected: PASS.
- [ ] **Step 5: Commit** `feat(state): lockfile v4 with a pending create journal`.

### Task 2: Journal core

**Files:**
- Create: `src/state/journal.rs` (types from Task 1 move here if not already), `pub mod journal;` in `src/state/mod.rs`

**Interfaces:**
- Consumes: Task 1 types; `crate::rt::BoxFuture`; `crate::state::store::StateStore` (Task 3 — in this task use a `persist: Arc<dyn Fn(&Lockfile) -> BoxFuture<'static, Result<()>> + Send + Sync>` seam that Task 3 fills).
- Produces:
  - `pub const LEASE_TTL_SECS: u64 = 600;`
  - `pub fn item<F: Future>(kind: &str, slug: &str, f: F) -> impl Future<Output = F::Output>` (task-local scope)
  - `pub fn match_fields(body: &serde_json::Value) -> BTreeMap<String, String>` (SHA-256 hex of canonical JSON per top-level key, skips `secrets`)
  - `#[derive(Clone)] pub struct Journal` with
    `pub fn new(committed: Lockfile, persist: PersistFn, now: fn() -> u64) -> Self`,
    `pub async fn begin(&self, body: &Value) -> Result<Option<u64>>` (None when no `item` in scope),
    `pub async fn finish(&self, seq: u64, id: u64) -> Result<()>`,
    `pub async fn abandon(&self, seq: u64) -> Result<()>`,
    `pub async fn commit(&self, lockfile: &Lockfile) -> Result<()>`,
    `pub fn run_id(&self) -> String`.

- [ ] **Step 1: Write failing tests** in `journal.rs`:
  - `begin_without_item_is_not_journaled` → `Ok(None)`, persist not called.
  - `begin_persists_committed_objects_plus_op` → persisted lockfile has the committed objects, one op with `id: None`, `run == run_id()`, `expires_at_unix == now + 600`.
  - `finish_sets_id_and_renews_lease`.
  - `commit_drops_ops_whose_entry_has_the_id_and_keeps_others` (two ops, lockfile records one) → persisted `pending.ops.len() == 1`; with both recorded → `pending == None`.
  - `abandon_removes_the_op`.
  - `secrets_never_hashed` → `match_fields(&json!({"name":"a","secrets":{"k":"v"}}))` has key `name` only.
  - `concurrent_begins_serialize` → 5 `begin`s joined concurrently produce `seq` 1..=5 and 5 persisted states each a superset of the last.
- [ ] **Step 2: Run** `cargo test --lib state::journal` — FAIL.
- [ ] **Step 3: Implement.** `run` = 32 hex chars from `getrandom`. `now` defaults to `web_time::SystemTime` unix secs. Use `futures::lock::Mutex` (works on wasm). Canonical JSON = the crate's existing canonicalizer used by `content_hash` (reuse, do not add one).
- [ ] **Step 4: Run** the gate — PASS.
- [ ] **Step 5: Commit** `feat(state): create journal with lease`.

### Task 3: `StateStore` + wiring into sync

**Files:**
- Create: `src/state/store.rs` (`pub mod store;`)
- Modify: `src/cli/sync/mod.rs:230` (load), `:801` and `:823` (saves), `src/cli/sync/embed.rs` (thread a store), `src/api/mod.rs` (`RossumClient` gets `journal: Option<Journal>` + `pub fn with_journal(self, j: Journal) -> Self`)

**Interfaces:**
- Produces: `pub trait StateStore: Send + Sync { fn hydrate<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>>; fn checkpoint<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>>; }`, `pub struct FsStateStore;` (both no-ops).
- Produces: `pub fn journal_for(paths: &Paths, committed: Lockfile, store: Arc<dyn StateStore>) -> Journal` — persist = `lockfile.save(&paths.lockfile())` then `store.checkpoint(paths)`.
- Produces: `embed::sync_logged_in_with_state(store, state: Arc<dyn StateStore>, root, env, token, opts, log)` and the same for watch; the existing functions call these with `Arc::new(FsStateStore)`.
- `sync::run` gains no public parameter; it reads the state store from a task-local `state::store::scope(state, fut)` defaulting to `FsStateStore`, so the CLI entry and `desktop/` do not change.

- [ ] **Step 1: Write failing test** `tests/embed_sync_mem.rs::checkpoint_called_after_every_lockfile_save` — a counting `StateStore` around a MemStore sync with one create: `hydrate` once, `checkpoint` ≥ 3 (begin, finish, commit).
- [ ] **Step 2: Run** it — FAIL.
- [ ] **Step 3: Implement**: `hydrate` before `Lockfile::load`; both saves become `journal.commit(&lockfile)`; client built `.with_journal(journal.clone())`.
- [ ] **Step 4: Byte-identity check**: run the full gate; additionally run `cargo test --test cli_sync --test embed_sync --test embed_sync_two_way` and confirm no assertion was edited. PASS.
- [ ] **Step 5: Commit** `feat(state): pluggable state store, journal wired into sync`.

### Task 4: Journal every create; guard create retries

**Files:**
- Modify: `src/api/mod.rs` create methods (lines ~212–268, incl. `create_hook_via_install`), `src/api/retry.rs`
- Modify (wrap the call in `journal::item(kind, slug, …)`): `src/cli/push/{labels.rs:77, rules.rs:85, hooks.rs:214, hooks.rs:256, queues.rs:99, schemas.rs:100, inboxes.rs:69, workspaces.rs:72, email_templates.rs:339, saved_views.rs:83, engines.rs:84, engine_fields.rs:82}`

**Interfaces:**
- Consumes: `Journal::{begin, finish, abandon}`, `journal::item`.
- Produces: `pub async fn find_create_match(&self, kind: &str, fields: &BTreeMap<String,String>, claimed: &HashSet<u64>) -> Result<Vec<u64>>` on `RossumClient` (used here and by Task 5), with `pub const IDENTITY_FIELDS: &[(&str, &[&str])]` in `src/state/journal.rs` — `name` plus each kind's parent reference (`queues`/`queue`/`workspace`/`schema`/`engine`, per the kind's create body).
- Produces: `retry::send_with_retry_opts(…, RetryOpts { gateway_5xx: bool })`; creates pass `false`.

- [ ] **Step 1: Write failing tests** (fake org, `tests/live/support/fake`): `every_create_kind_is_journaled` (for each of the 12 call sites a sync with one new object persists an op with the right `kind`/`slug` before the POST and an `id` after); `rejected_create_leaves_no_pending` (fake answers 400 → lockfile has no `pending`); `gateway_error_after_apply_does_not_duplicate` (fake applies the POST then answers 502 once → exactly one remote object, recorded id); `gateway_error_before_apply_resends` (502 without applying → one object).
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement** begin → POST → finish/abandon (abandon on 4xx only); on 502/503/504 call `find_create_match`: 1 → fetch and return it, 0 → resend, >1 → error naming ids.
- [ ] **Step 4: Run** the gate — PASS.
- [ ] **Step 5: Commit** `feat(push): journal creates and never blind-retry a create`.

### Task 5: Recovery and lease enforcement

**Files:**
- Create: `src/cli/sync/recover.rs`
- Modify: `src/cli/sync/mod.rs` (call after `Lockfile::load`), each push driver: extract its post-create "record + write canonical bytes" block into `pub(crate) fn complete_create(paths, lockfile, codec, created: &Value) -> Result<()>` and call it from both the driver and recovery

**Interfaces:**
- Consumes: Task 4 `find_create_match`, `Journal`, `stdin_coord::PromptRoute`.
- Produces: `pub async fn recover(paths: &Paths, lockfile: &mut Lockfile, client: &RossumClient, journal: &Journal, interactive: bool, now: u64) -> Result<RecoveryReport>`; `pub struct RecoveryReport { pub completed: Vec<(String, String, u64)>, pub dropped: Vec<(String, String)> }`.

- [ ] **Step 1: Write failing tests** (`recover.rs` unit + fake org):
  - `unexpired_foreign_lease_refuses_even_with_yes` — error contains the run id and the expiry time; no HTTP call made.
  - `own_or_expired_lease_is_taken_over`.
  - `op_with_id_completes_the_create` — after recovery the env file, base cache file and lockfile entry are byte-equal to an uninterrupted run's.
  - `op_with_id_whose_object_is_gone_is_dropped` (GET 404).
  - `op_without_id_and_one_match_is_adopted`, `…_no_match_is_dropped_and_reposted_once`.
  - `several_matches_prompt_interactively` (route answers the 2nd id → that id recorded) and `several_matches_stop_under_yes` (error lists every candidate id).
  - `recovers_on_a_machine_without_the_local_file`.
  - `two_machines_recover_to_the_same_id`.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement**; recovery calls `journal.commit` once at the end so the recovered state is durable before classify.
- [ ] **Step 4: Run** the gate — PASS.
- [ ] **Step 5: Commit** `feat(sync): recover unfinished creates before classifying`.

### Task 6: Crash matrix (interrupted runs)

**Files:**
- Modify: `tests/live/support/fake/mod.rs` (crash hook), Create: `tests/crash_matrix.rs`

**Interfaces:**
- Produces in the fake: `FakeOrg::crash_after(n: usize, point: CrashPoint)` where `enum CrashPoint { BeforeApply, AfterApplyBeforeResponse }`; at the point the fake signals a `tokio::sync::Notify` and delays the response forever. The test drops the sync future on that signal — exactly a closed tab or killed process.
- Drop points on the client side come from a test-only `StateStore` that fails or never returns at checkpoint N (crash between begin-persist and POST, and between response and finish-persist).

- [ ] **Step 1: Write the matrix** — for each create kind × each of the 4 points (before begin persist, after begin persist/before apply, after apply/before response, after finish persist/before driver write), plus: mid update batch (`PUSH_FANOUT` PATCHes, crash after 2 applied), mid delete phase, mid MDH row push, MDH collection and index create (`push/mdh.rs:340`, `:364`, `sync/execute.rs:4364` — not journaled because the name is the identity; this case proves it), during a pull. Each case: run 1 interrupted → run 2 (same tree) completes → `support::converge::assert_converged` (a third run plans nothing) → the fake holds exactly one object per slug. Plus `hook_secret_never_reaches_lockfile` (scan every persisted lockfile for the secret value).
- [ ] **Step 2: Run** `cargo test --test crash_matrix` — cases for creates FAIL on `main`'s behaviour if run against a pre-Task-4 build (sanity: temporarily disable journaling locally to see one fail, then restore); all PASS on the branch.
- [ ] **Step 3: Same matrix on a MemStore** (`embed::sync_logged_in_with_state`) — PASS.
- [ ] **Step 4: Run** the gate — PASS.
- [ ] **Step 5: Commit** `test(sync): interrupted-run matrix`.

### Task 7: `DataStorageStateStore`

**Files:**
- Create: `src/state/data_storage_store.rs`, `tests/live/support/fake/data_storage.rs` (in-memory `find`, `insert_many`, `replace_one` with filter + `upsert`, `delete_many`; duplicate-key error)
- Modify: `src/api/data_storage.rs` only if `replace_one` must expose matched count (see Task 9 fact 1)

**Interfaces:**
- Produces: `pub struct DataStorageStateStore`, `pub fn new(client: DataStorageClient, collection: String, key: String, chunk_bytes: usize) -> Self`, implementing `StateStore`.

- [ ] **Step 1: Write failing tests**: `hydrate_then_checkpoint_round_trips` (lockfile, base cache, conflict shadow byte-equal after a fresh MemStore hydrate); `large_file_is_chunked` (file > `chunk_bytes` → several blob docs, reassembled equal); `second_writer_loses_the_cas` (two stores, same key: second checkpoint errors with "changed by another run", manifest unchanged); `first_manifest_race_has_one_winner`; `crash_between_blobs_and_manifest_keeps_old_state` (drop after blob upload → hydrate returns the previous manifest's files); `unreferenced_blobs_are_collected`; `non_utf8_is_refused`.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement** per spec §6.
- [ ] **Step 4: Run** the crash matrix (Task 6) with this store under a MemStore — PASS. Then the gate.
- [ ] **Step 5: Commit** `feat(state): Data Storage state store`.

### Task 8: wasm API

**Files:**
- Modify: `wasm/src/lib.rs`, `wasm/src/bindings.rs`, `wasm/README.md`

**Interfaces:**
- Produces (TS): `useDataStorageState(opts: { collection?: string; key: string; chunkBytes?: number }): void` on `Project`; it applies to the next `sync`/`watch`.

- [ ] **Step 1: Write failing wasm-crate tests**: `sync_uses_data_storage_state_when_set`, `default_stays_in_memory` (no Data Storage request made).
- [ ] **Step 2: Run** `cd wasm && cargo test --locked` — FAIL.
- [ ] **Step 3: Implement**; document in the README's "Differences" list, including that `.rdc/state` keys then come from Data Storage, not `new Project(files)`. *(Added 2026-10-27, replacing the lease.)* `sync` and `watch` acquire an exclusive Web Lock (`navigator.locks.request`, name `rdc:<key>:<env>`, with `ifAvailable: true`) for their whole duration and fail with a clear "already running in another tab" error when it is held; this is the browser's `EnvLock` (spec §3). Also carry the state-store scope through `rt::rescope` (Task 3 deferred minor). Tests: a held lock makes a second `sync` fail; the lock is released after `sync` resolves and after it rejects.
- [ ] **Step 4: Run** the gate and `build-wasm` steps from `ci.yaml` — PASS.
- [ ] **Step 5: Commit** `feat(wasm): keep sync state in Data Storage`.

### Task 9: Live verification on the test org

**Files:**
- Create: `tests/live/scenarios/crash_recovery.rs` (+ wrappers per `tests/live/scenario_wrappers.rs` rules), entries in `tests/live/support/fake/quirks.rs` for each fact learned.

- [ ] **Step 1: Probe the open facts** (spec "Open facts") with a small live scenario each: `replace_one` response shape, document size limit (binary search up to 16 MiB, record the limit and set `chunk_bytes` under it), collection-name rules, token write access, and which create-body fields the server rewrites per kind (adjust `IDENTITY_FIELDS`). Record each as a `Modelled` quirk with a live citation.
- [ ] **Step 2: Live crash scenarios**: label, hook (with secret), queue+schema+inbox, rule — interrupt after apply/before response by dropping the future, rerun, `assert_converged`, and assert one object per name. Same for a `DataStorageStateStore` run.
- [ ] **Step 3: Run** `cargo test --test live -- --ignored --test-threads=1` with `RDC_LIVE_*` from the user — PASS. Then the offline gate.
- [ ] **Step 4: Commit** `test(live): crash recovery and Data Storage state against a real org`.

### Task 10: Docs

**Files:**
- Modify: `CLAUDE.md` (a bullet on the journal, the lease and the v4 bump), `src/cli/init.rs` scaffold text near line 1252 (what `pending` means in `.rdc/state/<env>.lock.json`), the spec's "Open facts" section with the answers from Task 9.

- [ ] **Step 1: Edit**, keeping every env-derived line inside its `rdc:` region (`CLAUDE.md` rules); run `cargo test --test cli_init` — PASS.
- [ ] **Step 2: Commit** `docs: crash-safe sync`.
