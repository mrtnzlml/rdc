# Desktop multi-env — Phase 4 (Configure) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development / executing-plans. Steps use `- [ ]`.

**Goal:** Finish the spec's Configure (⚙) slice: **remember** each project's promote direction + mirror + conflict policy (so day-to-day promotion is pick-and-go), and give a **⚙ Configure** affordance that reveals the underlying `.rdc/mapping.toml` and target `envs/<tgt>/overlay/` for advanced edits. Dart-only; no bridge/core changes.

**Architecture:** `Settings` gains a `promoteDefaults` map (project folder → {src, tgt, mirror, policy}). `AppState` restores a project's defaults when its Project view opens and persists them when the user changes direction/mirror/policy or prepares. The Promote panel gains a small **⚙** button that reveals `.rdc/mapping.toml` and `envs/<tgt>/overlay/` (creating the overlay dir if absent) via the existing `revealInFileManager` bridge fn + `dart:io` (same approach the Files tab already uses).

**Tech Stack:** Flutter/Dart only (`settings.dart`, `app_state.dart`, `home_page.dart`). No Rust, no FRB regen.

## Global Constraints
- Only `desktop/` changes; no customer names/identifiers anywhere; neutral placeholders.
- No bridge/core changes; no FRB regeneration.
- On-disk rdc contract untouched EXCEPT that ⚙ may `mkdir` an empty `envs/<tgt>/overlay/` so the user has a place to drop overlay files (a directory the CLI's migrate already reads; creating an empty one is inert).
- `~/.rdc-desktop/settings.json` stays backward compatible: a settings file without `promoteDefaults` loads fine (empty map).
- Baseline: `cd desktop && flutter analyze` clean, `flutter test` green.

---

### Task 1: Persist + restore promote defaults; ⚙ Configure reveal; README fix

**Files:**
- Modify: `desktop/lib/src/settings.dart`, `desktop/lib/src/app_state.dart`, `desktop/lib/src/home_page.dart`, `desktop/README.md`
- Test: `desktop/test/promote_defaults_test.dart` (new)

**Interfaces:**
- `Settings.promoteDefaults: Map<String, Map<String, dynamic>>` (folder → `{src, tgt, mirror, policy}`), round-tripped in `load`/`save`.
- `AppState.restorePromoteDefaults(String folder)` — populate `promoteSrc/promoteTgt/promoteMirror/promotePolicy` from settings for `folder` (no-op if none).
- `AppState.savePromoteDefaults(String folder)` — write the current promote direction/mirror/policy to settings for `folder`.
- `AppState.revealMapping(ProjectItem p)` / `revealOverlay(ProjectItem p, String tgt)`.

- [ ] **Step 1: Failing test** — `desktop/test/promote_defaults_test.dart`:
  ```dart
  import 'package:desktop/src/app_state.dart';
  import 'package:desktop/src/rust/api/rdc.dart';
  import 'package:desktop/src/settings.dart';
  import 'package:flutter_test/flutter_test.dart';

  ProjectItem _p(String folder, List<String> envs) => ProjectItem(
        ProjectSummary(id: folder, name: folder.split('/').last, folder: folder,
            envs: [for (final n in envs) EnvSummary(name: n, apiBase: 'https://x.test/api/v1',
                orgId: BigInt.one, authKind: AuthKind.token, lastSyncUnix: null, fileCount: BigInt.zero)]),
        false);

  void main() {
    test('promote defaults persist per project and restore on select', () {
      final s = Settings();
      final app = AppState(s);
      app.projects = [_p('/tmp/acme', ['dev', 'prod']), _p('/tmp/beta', ['dev', 'prod'])];
      app.selectProject('/tmp/acme');
      app.setPromoteDir('prod', 'dev'); // backport direction
      app.promoteMirror = true;
      app.promotePolicy = ConflictPolicy.useIncoming;
      app.savePromoteDefaults('/tmp/acme');
      // switch away then back
      app.selectProject('/tmp/beta');
      expect(app.promoteSrc, isNot('prod')); // beta has no saved default → not acme's
      app.selectProject('/tmp/acme');
      expect(app.promoteSrc, 'prod');
      expect(app.promoteTgt, 'dev');
      expect(app.promoteMirror, true);
      expect(app.promotePolicy, ConflictPolicy.useIncoming);
    });

    test('Settings round-trips promoteDefaults through JSON', () {
      final s = Settings();
      s.promoteDefaults['/tmp/acme'] = {'src': 'dev', 'tgt': 'prod', 'mirror': false, 'policy': 'keepTarget'};
      final json = s.toJson();
      final back = Settings.fromJson(json);
      expect(back.promoteDefaults['/tmp/acme']!['src'], 'dev');
      expect(back.promoteDefaults['/tmp/acme']!['policy'], 'keepTarget');
    });
  }
  ```
  (If `Settings` has no `toJson`/`fromJson`, add small pure helpers alongside `load`/`save` so this is unit-testable without touching disk.)

- [ ] **Step 2: Verify failure** — `cd desktop && flutter test test/promote_defaults_test.dart` → FAIL.

- [ ] **Step 3: Extend `Settings`** (`desktop/lib/src/settings.dart`):
  - Add field `Map<String, Map<String, dynamic>> promoteDefaults` (default `{}`), a `Map<String,dynamic> toJson()` and `static Settings fromJson(Map<String,dynamic>)` that include `promoteDefaults`, and have `load`/`save` delegate to `fromJson`/`toJson`. Load must tolerate a missing `promoteDefaults` key (→ `{}`) and a legacy file.

- [ ] **Step 4: `AppState` restore/save + reveal** (`desktop/lib/src/app_state.dart`):
  - Add `void setPromoteDir(String? src, String? tgt)` (sets both + notify) if not already present from Phase 3 (Phase 3 added `setPromoteDir`/`swapPromoteDir`).
  - `void restorePromoteDefaults(String folder)`: read `_settings.promoteDefaults[folder]`; if present set `promoteSrc/promoteTgt/promoteMirror` and map the `policy` string (`'useIncoming'|'keepTarget'|'skip'`) to `ConflictPolicy`; notify.
  - `void savePromoteDefaults(String folder)`: write `{src, tgt, mirror, policy: promotePolicy.name}` into `_settings.promoteDefaults[folder]`; `_settings.save()`.
  - In `selectProject`: after the existing folder-change `resetPromote(clearDirection: true)` (Phase 3), call `restorePromoteDefaults(folder)` so a saved direction re-arms (order: reset first, then restore).
  - `Future<void> revealMapping(ProjectItem p)`: reveal `<folder>/.rdc/mapping.toml` if it exists (via `revealInFileManager`), else reveal `<folder>` (fallback). `Future<void> revealOverlay(ProjectItem p, String tgt)`: ensure `<folder>/envs/<tgt>/overlay/` exists (`Directory(...).createSync(recursive: true)` via `dart:io`), then reveal it.
  - Call `savePromoteDefaults(p.folder)` inside `preparePromote` (persist the direction the user actually prepared) and when the user changes mirror/policy in the panel (the UI can call it, or persist on prepare only — prepare-only is simplest and sufficient).

- [ ] **Step 5: ⚙ Configure UI** (`desktop/lib/src/home_page.dart`, in `_PromotePanel`):
  - Add a small **⚙ Configure** button/menu near the From⇄To row. On tap show a lightweight menu (or two buttons): "Reveal mapping (.rdc/mapping.toml)" → `state.revealMapping(item)`; "Reveal target overlay (envs/<tgt>/overlay)" → `state.revealOverlay(item, tgt)` (enabled only when a tgt is selected). Keep it visually minor (an icon button with a tooltip is fine).

- [ ] **Step 6: README clarification** (`desktop/README.md`): adjust the Promote paragraph so it's clear that Prepare's `migrate` step is offline but the **dry-run preview contacts the target org** (needs the target token) — remote is only *written* at Push. One or two words; don't overhaul.

- [ ] **Step 7: Verify** — `cd desktop && dart analyze lib/` clean; `flutter test test/promote_defaults_test.dart test/promote_test.dart test/project_view_test.dart test/app_state_test.dart` green. (Golden regen is Task 2.)

- [ ] **Step 8: Commit** — `git add desktop/lib/src/settings.dart desktop/lib/src/app_state.dart desktop/lib/src/home_page.dart desktop/README.md desktop/test/promote_defaults_test.dart && git commit -m "feat(desktop): remember promote defaults + Configure reveal (mapping/overlay)"`

---

### Task 2: Golden + whole-suite verification

**Files:** `desktop/test/golden_mdh_test.dart` if the ⚙ button shifts the project golden; regenerate `mdh_project_light.png` if needed.

- [ ] **Step 1** — Run `cd desktop && flutter test test/golden_mdh_test.dart`. If the `project view — light` golden now fails because the ⚙ button changed the panel layout, regenerate with `--update-goldens`, then confirm green without the flag and visually verify the ⚙ affordance appears. If it doesn't change (e.g. the ⚙ is inside existing space), no regen needed.
- [ ] **Step 2: Whole suite** — `cd desktop && flutter analyze` clean; `flutter test` all green; `cd desktop/rust && cargo test` green.
- [ ] **Step 3** — Attempt `flutter test integration_test -d macos`; report (no new integration test — Configure is local file reveal + settings persistence, unit-tested).
- [ ] **Step 4: Commit** — `git add desktop/test && git commit -m "test(desktop): refresh project golden for Configure affordance"` (skip if no golden changed).

---

## Self-review notes
- Spec coverage: §10 Configure v1 = remembered per-path UI defaults (Task 1 Settings + restore/save) + reveal `.rdc/mapping.toml` / `envs/<tgt>/overlay/` (Task 1 ⚙). No in-GUI mapping/overlay editor (deferred, per spec). Also folds in the Phase-3-review README clarification.
- Backward compat: settings.json without `promoteDefaults` loads to `{}`; ⚙ overlay `mkdir` is inert (empty dir migrate already tolerates).
- Type consistency: policy persisted as `ConflictPolicy.name` string (`useIncoming`/`keepTarget`/`skip`); restore maps back. `revealOverlay` uses `dart:io` `Directory.createSync` (same direct-fs approach as the Files tab), then the `revealInFileManager` bridge fn.
- **Deferred** (from the whole-branch review, 2026-08-04): spec §9.1's full drift-detection (compare `envs/<tgt>` to its base/lockfile and only warn when the target actually has un-synced local changes) and the `DriftStatus` model field are not implemented — Prepare always overwrites `envs/<tgt>/` unconditionally. v1 mitigates this with an always-visible caption next to the Prepare button ("Prepare rewrites `<tgt>`'s local files from `<src>` and replaces any un-synced local edits to `<tgt>`. Nothing is pushed until you Push.") rather than a conditional warning dialog. Real drift-detection is left to a follow-up.
