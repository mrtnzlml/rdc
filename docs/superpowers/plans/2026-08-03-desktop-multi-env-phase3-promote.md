# Desktop multi-env — Phase 3 (in-app promote) Implementation Plan

> **SUPERSEDED 2026-09-01:** Promote was removed from the desktop app; do not build `prepare_promotion`, `push_promotion`, `ConflictPolicy`, or the Promote panel from this plan. See `docs/superpowers/specs/2026-09-01-desktop-watch-and-promote-removal-design.md`.

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development / executing-plans. Steps use `- [ ]`.

**Goal:** Promote configuration between two of a project's environments, in either direction, from the Project view: offline **Prepare** (`migrate`) → reviewable **Preview** (dry-run push) → gated non-interactive **Push** (`sync --no-pull`). This is the app's first remote-write path.

**Architecture:** Two new **embed seams** are added to the `rdc` core (the same mechanism `src/cli/sync/embed.rs` already provides for the desktop app; both changes are non-behavioral): `migrate::run_at(cwd, …)` (a pure cwd-explicit refactor of the existing `migrate::run`) and `sync::embed::sync_push_logged(…)` (wraps the existing `run_cycle` in `--no-pull` mode). The bridge adds `prepare_promotion` (runs migrate offline, then a dry-run push whose rendered plan is captured as the preview) and `push_promotion` (runs the real `--no-pull` push, streaming the log). The UI adds a Promote panel to the Project view.

**Tech Stack:** Rust core (`src/cli/migrate`, `src/cli/sync/embed.rs`), bridge (`desktop/rust`), FRB 2.12.0, Flutter/Dart.

## Global Constraints
- The CLI's **behavior** is unchanged. The only core edits are: (a) extracting `migrate::run`'s body into `run_at(cwd, …)` with `run` delegating to it; (b) a new `sync_push_logged` embed wrapper. No CLI command, flag, or output changes. Everything else lives under `desktop/`.
- No customer names/identifiers anywhere (code, tests, commit messages); neutral placeholders.
- **Conflict-polarity inversion (critical — a unit test pins it):** in a promote push (`--no-pull`, pushing local `envs/<tgt>` to the target org), the app's user label maps INVERTED to the CLI flag: **"Use incoming"** (promoted config wins) → `ConflictStrategy::KeepLocal`; **"Keep target"** (target org wins) → `ConflictStrategy::UseRemote`; **"Skip"** → `ConflictStrategy::Skip`.
- Env-level Sync remains pull-only; promote is the ONLY remote-write path, and it is gated (explicit Push button + policy + allow-deletes).
- Prepare mutates local `envs/<tgt>/` (migrate). Remote is untouched until Push. UI copy must say so.
- After Rust `api` changes: regenerate FRB bindings + commit generated files.
- Baseline: `cargo test` (workspace + `desktop/rust`) green; `cd desktop && flutter analyze` clean, `flutter test` green.

---

### Task 1: Core embed seams — `migrate::run_at` + `sync::embed::sync_push_logged`

**Files:**
- Modify: `src/cli/migrate/mod.rs` (extract `run_at`)
- Modify: `src/cli/sync/embed.rs` (add `sync_push_logged`)
- Verify `src/cli/mod.rs` / `src/lib.rs` expose `cli::migrate` publicly (the bridge already reaches `rdc::cli::sync::embed` and `rdc::cli::init`).

**Interfaces produced:**
- `pub fn run_at(cwd: &Path, src: &str, tgt: &str, mirror: bool, dry_run: bool, only: Vec<String>, migrate_score_thresholds: bool) -> Result<()>` in `crate::cli::migrate` (and `run` delegates to it).
- `pub async fn sync_push_logged(cwd: &Path, env: &str, token: &str, conflict: Option<ConflictStrategy>, allow_deletes: bool, dry_run: bool, log_sink: Box<dyn std::io::Write + Send>) -> Result<CycleOutcome>` in `crate::cli::sync::embed`.

- [ ] **Step 1: Failing test (migrate cwd-explicit)** — add to `src/cli/migrate/mod.rs` tests (or a new test) a case that builds a tiny two-env project in a tempdir and calls `run_at(tmp.path(), "dev", "prod", false, true /*dry_run*/, vec![], false)` and asserts `Ok(())` WITHOUT changing the process cwd. (Dry-run so it makes no writes; the point is proving `run_at` takes an explicit cwd.) Run: `cargo test -p rdc migrate` → FAIL (no `run_at`).

- [ ] **Step 2: Extract `run_at`** — in `src/cli/migrate/mod.rs`, rename the existing `pub fn run(src, tgt, mirror, dry_run, only, migrate_score_thresholds)` body into:
  ```rust
  pub fn run_at(
      cwd: &std::path::Path,
      src: &str, tgt: &str, mirror: bool, dry_run: bool,
      only: Vec<String>, migrate_score_thresholds: bool,
  ) -> Result<()> {
      // ... the existing body, but replace the line
      //   let cwd = std::env::current_dir().context("getting current directory")?;
      // with using the `cwd` parameter (drop that line; keep everything else).
      ...
  }

  pub fn run(src: &str, tgt: &str, mirror: bool, dry_run: bool, only: Vec<String>, migrate_score_thresholds: bool) -> Result<()> {
      let cwd = std::env::current_dir().context("getting current directory")?;
      run_at(&cwd, src, tgt, mirror, dry_run, only, migrate_score_thresholds)
  }
  ```
  The only change is where `cwd` comes from — no logic change. Verify the CLI call site (`src/cli/mod.rs` Migrate arm) still calls `run(...)` unchanged.

- [ ] **Step 3: Add `sync_push_logged`** — in `src/cli/sync/embed.rs` (mirror `sync_no_push_logged`, but push mode):
  ```rust
  use crate::cli::resolve::ConflictStrategy;

  /// Run one `--no-pull` (deploy) reconciliation cycle, streaming rdc's rendered
  /// log into `log_sink`. `dry_run` renders the plan and stops before executing.
  /// `conflict` selects the non-interactive BothDiverged strategy; `allow_deletes`
  /// permits local-tombstone → remote DELETE. Pull is never performed (local files
  /// are never overwritten). Used by the desktop app's promote Push.
  pub async fn sync_push_logged(
      cwd: &Path,
      env: &str,
      token: &str,
      conflict: Option<ConflictStrategy>,
      allow_deletes: bool,
      dry_run: bool,
      log_sink: Box<dyn std::io::Write + Send>,
  ) -> Result<CycleOutcome> {
      let paths = crate::paths::Paths::for_env(cwd, env);
      let _lock = crate::cli::sync::lock::EnvLock::acquire(
          &paths.env_lock(), std::time::Duration::from_secs(30))?;
      let renderer = Log::for_sink(crate::cli::resolve::ColorMode::Color, log_sink);
      crate::cli::sync::run_cycle(
          env,
          false,          // interactive
          dry_run,
          allow_deletes,
          false,          // no_push
          true,           // no_pull  <-- deploy: push local, never overwrite local
          conflict,
          Some(renderer),
          Some(cwd),
          Some(token.to_string()),
      ).await
  }
  ```

- [ ] **Step 4: Verify** — `cargo test -p rdc migrate` green; `cargo build -p rdc` clean. (The desktop bridge is a separate workspace; it builds in Task 2.)
- [ ] **Step 5: Commit** — `git add src/cli/migrate/mod.rs src/cli/sync/embed.rs && git commit -m "feat(core): embed seams for promote — migrate::run_at + sync_push_logged"`

---

### Task 2: Bridge `prepare_promotion` / `push_promotion`

**Files:**
- Modify: `desktop/rust/src/api/rdc.rs`
- Regenerate: `desktop/lib/src/rust/**`, `desktop/rust/src/frb_generated.rs`

**Interfaces produced (Dart names in parens):**
- `enum ConflictPolicy { UseIncoming, KeepTarget, Skip }` (`ConflictPolicy`)
- `struct PromotionPreview { plan: Vec<String> }` (`PromotionPreview`) — the captured, rendered dry-run plan lines
- `fn prepare_promotion(folder: String, src: String, tgt: String, mirror: bool) -> Result<PromotionPreview>` (`preparePromotion`)
- `fn push_promotion(folder: String, src: String, tgt: String, policy: ConflictPolicy, allow_deletes: bool, sink: StreamSink<SyncPhase>) -> Result<()>` (`pushPromotion`)

- [ ] **Step 1: Failing test — polarity mapping** — append to `api/rdc.rs` tests:
  ```rust
  #[test]
  fn conflict_policy_maps_inverted_for_promote_push() {
      use crate::api::rdc::{policy_to_strategy, ConflictPolicy};
      use rdc::cli::resolve::ConflictStrategy;
      assert!(matches!(policy_to_strategy(ConflictPolicy::UseIncoming), Some(ConflictStrategy::KeepLocal)));
      assert!(matches!(policy_to_strategy(ConflictPolicy::KeepTarget), Some(ConflictStrategy::UseRemote)));
      assert!(matches!(policy_to_strategy(ConflictPolicy::Skip), Some(ConflictStrategy::Skip)));
  }
  ```
  Run `cd desktop/rust && cargo test conflict_policy` → FAIL.

- [ ] **Step 2: Implement** — in `desktop/rust/src/api/rdc.rs`:
  ```rust
  #[derive(Debug, Clone, Copy)]
  pub enum ConflictPolicy { UseIncoming, KeepTarget, Skip }

  /// The critical polarity inversion (see plan Global Constraints).
  pub(crate) fn policy_to_strategy(p: ConflictPolicy) -> Option<rdc::cli::resolve::ConflictStrategy> {
      use rdc::cli::resolve::ConflictStrategy as S;
      Some(match p {
          ConflictPolicy::UseIncoming => S::KeepLocal, // promoted (local) wins
          ConflictPolicy::KeepTarget  => S::UseRemote, // target (remote) wins
          ConflictPolicy::Skip        => S::Skip,
      })
  }

  #[derive(Debug, Clone)]
  pub struct PromotionPreview { pub plan: Vec<String> }

  /// Offline `migrate src -> tgt` (writes local `envs/<tgt>/`), then a dry-run
  /// `--no-pull` push whose rendered plan is captured and returned. Needs the
  /// TARGET env's token (the dry-run push lists/scans the target remote). Nothing
  /// is written to the remote.
  pub fn prepare_promotion(folder: String, src: String, tgt: String, mirror: bool) -> Result<PromotionPreview> {
      let folder = PathBuf::from(folder);
      // 1) offline migrate (no token, no network)
      rdc::cli::migrate::run_at(&folder, &src, &tgt, mirror, false /*dry_run*/, vec![], false)
          .map_err(|e| anyhow!("{e:#}"))?;
      // 2) dry-run push preview against the target remote
      let api_base = env_api_base(&folder, &tgt)?;
      let collector = LineCollector::default();
      let lines = collector.lines.clone();
      block_on(async {
          let token = rdc::secrets::resolve_token(&folder, &tgt, &api_base).await?;
          rdc::cli::sync::embed::sync_push_logged(
              &folder, &tgt, &token, None /*conflict*/, false /*allow_deletes*/, true /*dry_run*/,
              Box::new(collector),
          ).await
      }).map_err(|e| anyhow!("{e:#}"))?;
      let plan = lines.lock().unwrap().clone();
      Ok(PromotionPreview { plan })
  }

  /// The real gated push: `--no-pull` with the chosen conflict policy and
  /// allow-deletes, streaming the log. Needs the TARGET token.
  pub fn push_promotion(folder: String, src: String, tgt: String, policy: ConflictPolicy, allow_deletes: bool, sink: StreamSink<SyncPhase>) -> Result<()> {
      let folder = PathBuf::from(folder);
      let _ = sink.add(SyncPhase::Started);
      let forwarder = LineForwarder { sink: sink.clone(), buf: Vec::new() };
      let result: Result<CycleOutcome> = block_on(async {
          let api_base = env_api_base(&folder, &tgt)?;
          let token = rdc::secrets::resolve_token(&folder, &tgt, &api_base).await?;
          // Re-run migrate so the target snapshot reflects src at push time
          // (Prepare may have been a while ago / src re-synced since).
          rdc::cli::migrate::run_at(&folder, &src, &tgt, false, false, vec![], false)
              .map_err(|e| anyhow!("{e:#}"))?;
          rdc::cli::sync::embed::sync_push_logged(
              &folder, &tgt, &token, policy_to_strategy(policy), allow_deletes, false /*dry_run*/,
              Box::new(forwarder),
          ).await
      });
      match result {
          Ok(o) => {
              let _ = sink.add(SyncPhase::Log { line: format!("✓ promote done · {} pushed · {} conflicts", o.items_pushed, o.conflicts) });
              let _ = sink.add(SyncPhase::Done { file_count: o.items_pushed as u64 });
          }
          Err(e) => { let _ = sink.add(SyncPhase::Error { message: format!("{e:#}") }); }
      }
      Ok(())
  }
  ```
  Add the small helpers: `env_api_base(folder, env) -> Result<String>` (loads `rdc.toml`, returns that env's `api_base` or a clear error), and `LineCollector` (a `std::io::Write` accumulating whole lines into an `Arc<Mutex<Vec<String>>>`, mirroring `LineForwarder`'s line-splitting but pushing into the vec). NOTE: `mirror` for `push_promotion` is intentionally `false` in the re-migrate (Prepare's `mirror` governs preview; a mirror push would need its own gated flag — deferred; document it).

  Correction to keep push honest: thread `mirror` into `push_promotion` too (add a `mirror: bool` param) so the pushed migrate matches the previewed one. Update the signature to `push_promotion(folder, src, tgt, mirror, policy, allow_deletes, sink)` and pass `mirror` to `run_at`.

- [ ] **Step 3: Verify** — `cd desktop/rust && cargo test` green (polarity test + existing). 
- [ ] **Step 4: Regenerate bindings** — `cd desktop && flutter_rust_bridge_codegen generate`; confirm `preparePromotion`/`pushPromotion`/`ConflictPolicy`/`PromotionPreview` in `rdc.dart`.
- [ ] **Step 5: Commit** — `git add desktop/rust/src/api/rdc.rs desktop/lib/src/rust desktop/rust/src/frb_generated.rs && git commit -m "feat(desktop): bridge prepare_promotion / push_promotion (gated, inverted-polarity)"`

---

### Task 3: Promote panel in the Project view + AppState wiring

**Files:**
- Modify: `desktop/lib/src/app_state.dart` (promote state machine), `desktop/lib/src/home_page.dart` (`_ProjectView` gets a Promote panel)
- Test: `desktop/test/promote_test.dart` (new)

**Interfaces:** `preparePromotion({folder, src, tgt, mirror}) -> PromotionPreview`, `pushPromotion({folder, src, tgt, mirror, policy, allowDeletes}) -> Stream<SyncPhase>`, `ConflictPolicy`, `PromotionPreview`.

- [ ] **Step 1: AppState promote state** — add to `AppState`:
  - fields: `String? promoteSrc, promoteTgt; PromotionPreview? promotePreview; bool promoteMirror = false; bool promoteAllowDeletes = false; ConflictPolicy promotePolicy = ConflictPolicy.keepTarget; List<String> promoteLog = []; enum PromoteStage { idle, preparing, preview, pushing, done, error } promoteStage = idle; String? promoteError;`
  - `Future<void> prepare(ProjectItem p)` — sets stage=preparing, calls `preparePromotion(folder: p.folder, src: promoteSrc!, tgt: promoteTgt!, mirror: promoteMirror)`, stores preview, stage=preview (or error).
  - `void push(ProjectItem p)` — stage=pushing, `promoteLog=[]`, listens to `pushPromotion(...)` stream, appends log lines, on Done → stage=done + `reload()`, on Error → stage=error.
  - `void resetPromote()` / `void setPromoteDir(src, tgt)` / swap helper.
  - Write a unit test (`promote_test.dart`) for the pure state transitions you CAN test without the bridge (direction set/swap, policy default = keepTarget, reset). The bridge-backed prepare/push are covered by the integration test in Task 4 + manual live verification.

- [ ] **Step 2: Promote panel UI** — in `_ProjectView` (below the Environments table), add a `_PromotePanel` (only when the project has ≥2 envs):
  - Row: `From [src ▾]  ⇄  To [tgt ▾]` (dropdowns over `envs`, swap button flips), a `Mirror` checkbox, and a **Prepare →** button (disabled unless src≠tgt).
  - On `promoteStage == preview`: show the captured `promotePreview.plan` lines (reuse the `_SyncLogCard`/ansi rendering for colored plan text), a **conflict policy** segmented control (Use incoming · Keep target · Skip; default Keep target), an **Allow deletes** checkbox, and **Push to <tgt> →** + **Cancel**. Copy: "Prepared locally — nothing pushed yet."
  - On `promoteStage == pushing/done/error`: show the streaming `promoteLog` (ansi) + status.
  - Wire buttons to `state.prepare(p)` / `state.push(p)` / `state.resetPromote()`.
  - Widget test (`promote_test.dart`): render `_ProjectView` for a ≥2-env project, assert the From/To pickers + Prepare button render; assert the panel is absent for a 1-env project.

- [ ] **Step 3: Verify** — `cd desktop && dart analyze lib/` clean; `flutter test test/promote_test.dart test/project_view_test.dart test/app_state_test.dart` green. (Do NOT run full `flutter test` — golden update is Task 4.)
- [ ] **Step 4: Commit** — `git add desktop/lib/src/app_state.dart desktop/lib/src/home_page.dart desktop/test/promote_test.dart && git commit -m "feat(desktop): promote panel (From⇄To → Prepare → preview+policy → Push)"`

---

### Task 4: Goldens + verification

**Files:** `desktop/test/golden_mdh_test.dart` (Project-view golden now includes the Promote panel — regenerate `mdh_project_light.png`), and a short note in `desktop/README.md` on the promote flow.

- [ ] **Step 1** — The existing `project view — light` golden now renders the Promote panel (≥2-env project). Regenerate: `cd desktop && flutter test --update-goldens test/golden_mdh_test.dart`, then confirm green without the flag. Visually confirm the panel (From/To + Prepare) appears below the table.
- [ ] **Step 2** — `cd desktop && flutter analyze` clean; `flutter test` all green; `cd desktop/rust && cargo test` green; `cargo test -p rdc` green (core seams).
- [ ] **Step 3** — Attempt `flutter test integration_test -d macos`; report. (Promote's prepare/push need a live target remote, so the integration test does NOT perform a live push — it asserts `preparePromotion` against an unreachable target returns a clear error, and the polarity unit test covers the mapping. Live promote is verified manually against real dev/test envs.)
- [ ] **Step 4** — Add a short "Promote" paragraph to `desktop/README.md` (2-phase: offline Prepare + gated Push; pull-only otherwise).
- [ ] **Step 5: Commit** — `git add desktop/test desktop/README.md && git commit -m "test/docs(desktop): promote golden + README"`

---

## Self-review notes
- Spec coverage: §7 promote (From⇄To, Prepare/Preview/Push) → Tasks 2–3; §8 bridge ops (`prepare_promotion`/`push_promotion`) → Task 2; §8.1 conflict-polarity inversion → Task 2 (pinned by `conflict_policy_maps_inverted…`); §9 2-phase + target-token auth → Task 2; pull-only-except-promote invariant preserved (env Sync untouched).
- Deviations/decisions: preview is the **captured rendered dry-run plan** (text), not a structured object — sidesteps the spec §4 "does dry-run yield a structured breakdown" unknown; conflict policy + allow-deletes are always-present controls with SAFE defaults (Keep target / deletes off), so gating doesn't depend on an uncertain dry-run count. Drift-clobber guard (§9.1) and Configure (§10) are Phase 4.
- Risk: `push_promotion` re-runs migrate before pushing so the pushed snapshot matches src at push time; `mirror` is threaded through both prepare and push so preview == push.
