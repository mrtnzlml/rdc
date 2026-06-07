# `rdc migrate` + remove `deploy` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. TDD (RED→GREEN); full suite + `clippy -D warnings` + `rustfmt --check` stay green; commit per task with trailer `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`; never `git push`; no customer names in code/tests.

**Goal:** Replace `rdc deploy` with a pure-local `rdc migrate <src> <tgt>` (snapshot→snapshot transform) + the already-dependency-ordered `rdc sync` push, per spec `docs/superpowers/specs/2026-06-05-portable-snapshots-migrate-sync-design.md` §4–§5.

**Architecture:** Snapshots are env-portable (`rdc://<kind>/<slug>` refs, Stage 1 — done + live-verified). `migrate` copies `envs/<src>/` → `envs/<tgt>/`, renaming slugs per the `Mapping` and substituting `rdc://<kind>/<src_slug>`→`rdc://<kind>/<tgt_slug>` in content (identity for auto-matched same-slug pairs). No network, no URLs, no lockfile. Then `rdc sync <tgt>` reconciles (its push already creates objects in dependency order). `deploy` stays intact until Stage C removes it.

**Tech stack:** Rust; reuse `Mapping` (src/mapping.rs), `deploy/map.rs` enumerators + `auto_match` + `validate_mapping_sources`, `Overlay` (src/overlay.rs), `snapshot::refs::walk_strings_mut`, `Paths` (src/paths.rs).

---

## Stage A — `rdc migrate` (new module, deploy untouched)

### Task A1: snapshot enumeration + tgt-path remap
**Files:** Create `src/cli/migrate/mod.rs`, `src/cli/migrate/paths.rs` (or inline). Test: same file `#[cfg(test)]`.
- Enumerate every file under `src_paths.env_root()` recursively (skip `_index.md` — sync rebuilds it; skip nothing else).
- `fn remap_relative(rel: &Path, mapping: &Mapping) -> PathBuf` — remap structured slug path-components to tgt slugs:
  - `hooks/<slug>.{json,py}` → `hooks/<tgt(hooks,slug)>.…`
  - `labels/<slug>.json`, `rules/<slug>.{json,py}` likewise.
  - `workspaces/<ws>/…` → `workspaces/<tgt(workspaces,ws)>/…`; within it `queues/<q>/…` → `queues/<tgt(queues,q)>/…`; leaf `queue.json|schema.json|inbox.json` unchanged; `email-templates/<t>.json` → remap via `email_templates` compound key `<ws>/<q>/<t>`.
  - `workflows/<wf>/workflow.json` + `steps/<s>.json` — workflows pull-only (no Mapping section) → identity.
  - `mdh/<dataset>/indexes.json` — identity (datasets match by slug).
  - Unmapped slug → identity (same slug).
- **Step 1 (RED):** test `remap_relative` for a renamed hook + renamed queue-under-workspace + identity label. **Step 2:** implement. **Step 3 (GREEN).** **Commit.**

### Task A2: ref substitution + overlay + write
**Files:** `src/cli/migrate/mod.rs`. Test: same file.
- `fn build_subst(mapping) -> BTreeMap<String,String>`: for every mapped pair across portable kinds, `rdc://<kind>/<src>` → `rdc://<kind>/<tgt>` (skip identity pairs).
- For each enumerated `.json`: parse → `walk_strings_mut` replacing whole-string `rdc://` keys via the subst dict → apply tgt overlay for `(kind, tgt_slug)` (reuse `Overlay::load` + `apply_overrides`) → `write_atomic` to the remapped tgt path. For `.py`/other: copy bytes verbatim to remapped path.
- **Step 1 (RED):** test a queue.json whose `workspace`/`schema`/`hooks` refs are rewritten when those slugs are renamed, and left identical when not. **Step 2:** implement. **Step 3 (GREEN).** **Commit.**

### Task A3: `migrate::run` orchestration
**Files:** `src/cli/migrate/mod.rs`.
- `pub fn run(src: &str, tgt: &str, mirror: bool, dry_run: bool, only: Vec<String>) -> Result<()>` (sync — no network):
  1. `Paths::for_env` src+tgt; load `Mapping::load(src_paths.mapping_file(src,tgt))`.
  2. `map::auto_match(&mut mapping, &src, &tgt)`; `map::validate_mapping_sources(&mapping, &src, file)`; persist mapping (`mapping.save`).
  3. `--only`: filter enumerated objects via `deploy::selection` dep-check (reuse).
  4. transform (A1+A2). `--mirror`: delete tgt-only objects (files present in tgt, absent in src+mapping). `--dry-run`: print plan, no writes.
  5. Print summary: N copied, N renamed, N pruned; remind to review `git diff` then `rdc sync <tgt>`.
- **Steps:** RED integration test (build a 2-env temp project, migrate, assert tgt files/refs) → implement → GREEN → **commit.**

### Task A4: CLI wiring
**Files:** `src/cli/mod.rs` (add `Migrate { src, tgt, mirror, dry_run, only }` arm + dispatch via `env_picker`), keep `Deploy` arm.
- **Steps:** RED (a `cli_migrate.rs` integration test invoking the binary) → wire → GREEN → **commit.**

### Task A5: live local verification (pure-local, no target org needed)
- In `/tmp/rdc-coll2`: `rdc init --env stage=…` (any api_base/org placeholder), then `rdc migrate test stage`; assert `envs/stage/` mirrors `envs/test/` rdc:// refs (identity), overlays applied, no network call. Document in notes.

---

## Stage B — confirm sync push subsumes deploy (gap-fill only)
Audit that sync push covers every create/apply path deploy had (store_extensions install, hook_secrets injection, mdh index reconcile, `--only`/`--mirror`/`--dry-run`/`--force-overwrite-drift`). For each gap, add to sync push with TDD. (Spec §5 table.) Likely small — push/*.rs already create + the drivers exist.

## Stage C — remove `deploy` + delete `rewrite_urls`
- Replace `Deploy` clap arm with a guiding-error stub (spec §4.3 text). Relocate any deploy-only machinery still needed (enumerators already used by migrate; store_extensions/hook_secrets/mdh into sync push or migrate per §5). Delete `deploy/common.rs::{rewrite_urls, walk_strings_mut}` (latter already lifted to snapshot::refs). Rewrite `tests/cli_deploy.rs` → `tests/cli_migrate.rs` + sync-push assertions (spec §9.6). Suite green, `dead_code=deny` clean.

## Stage D — lockfile v3 (smallest once deploy gone)
- After Stage C, the heavy `url` consumers (deploy/) are deleted. Re-survey `.url` sites; if few remain (push resolve via `url_for_slug`), derive `{api_base}/{endpoint}/{id}` with the endpoint map (kind==endpoint except `organization`→`organizations`; mdh keeps its data-storage URL — store or special-case). Bump lockfile v2→v3, drop persisted `url`, `load` tolerates legacy. Re-baseline self-heals. **Reassess value vs. churn before doing; may keep `url` if derivation stays unsafe for mdh.**
