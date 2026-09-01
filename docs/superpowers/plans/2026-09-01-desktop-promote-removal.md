# Desktop Promote Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the in-app Promote feature from the rdc desktop app, so the only path that writes configuration from one Rossum environment into another is the GitLab CI deploy job, which gates it on `pytest`.

**Architecture:** Deletion, in dependency order — Dart UI first, then Dart state and settings, then the Rust bridge functions, then regenerate the flutter_rust_bridge bindings. Deleting Dart before Rust keeps the tree compiling at every commit: unused `pub` bridge functions are legal, unused Dart references are not.

**Tech Stack:** Flutter/Dart (`desktop/`), Rust (`desktop/rust`, crate `rdc_bridge`), flutter_rust_bridge 2.12.0.

**Spec:** `docs/superpowers/specs/2026-09-01-desktop-watch-and-promote-removal-design.md` (§5, §9)

## Global Constraints

- **Never put customer names or customer-specific identifiers** — org/division/region codes, real environment names, queue/engine/hook slugs, hostnames, URLs, file paths — anywhere in this repository, including commit messages. Use neutral placeholders (`acme`, `main`, `invoices`, `dev`/`test`/`prod`). This is a repo rule from `CLAUDE.md`, not advice.
- **Never `git push`.** Commit to local `main` only. The user publishes.
- **Work on `main`.** Do not create a `fix/` or `work/` branch.
- **Do not run repo-wide `cargo fmt`.** This repo is not fmt-clean under the local rustfmt; a fmt-check failure is pre-existing, not a regression.
- **The on-disk rdc contract is untouched by this plan.** `rdc.toml`, `secrets/`, `envs/`, `.rdc/state` must be byte-identical in behaviour before and after. No task here may change them.
- **flutter_rust_bridge is pinned at exactly 2.12.0.** Do not upgrade it. `flutter_rust_bridge_codegen --version` must print `2.12.0` before you regenerate anything.
- **The `rdc` core crate is not modified by this plan.** `src/cli/sync/embed.rs::sync_push_logged` becomes uncalled at Task 4 and stays in the tree; it is a `pub` item in a lib, so it produces no dead-code warning. Its removal belongs to the follow-on plan `2026-09-01-desktop-two-way-sync-and-watch.md`, which replaces it.
- Someone else may be working in this same checkout. Never run bare `git stash` / `git checkout` / `git reset` / `git clean`. Only add and commit the paths a task names.

---

## File Structure

| File | Responsibility after this plan |
|---|---|
| `desktop/lib/src/home_page.dart` | Project view renders the Environments table only. All six promote widgets gone. |
| `desktop/lib/src/app_state.dart` | Sync + selection + env-management state. No promote state, no `.rdc/mapping.toml` or overlay reveal helpers. |
| `desktop/lib/src/settings.dart` | `parentFolder` + `externalPaths` + **preserved unknown keys**. No `promoteDefaults`. |
| `desktop/rust/src/api/rdc.rs` | Bridge: list/add/validate/edit/add-env/remove-env/rename-env/sync/trash/reveal. No promote, no `migrate`. |
| `desktop/lib/src/rust/**`, `desktop/rust/src/frb_generated.rs` | Regenerated bindings (committed). |
| `desktop/test/promote_test.dart`, `desktop/test/promote_defaults_test.dart` | Deleted. |
| `desktop/test/goldens/mdh_project_light.png` | Regenerated without the Promote panel. |
| `desktop/README.md` | No Promote section. |

---

### Task 1: Delete the Promote UI from the Project view

**Files:**
- Modify: `desktop/lib/src/home_page.dart`

**Interfaces:**
- Consumes: nothing.
- Produces: `_ProjectView` renders `_ProjectBar` + `_SectionTitle('Environments')` + `_EnvTable` and nothing else. `AppState`'s promote members are still present and still compile; Task 2 removes them.

- [ ] **Step 1: Delete the promote-panel widget section**

Delete everything from the section banner comment

```dart
// ------------------------------------------------------------ promote panel
```

down to and including the closing brace of `_PromotePushPanel`, i.e. the whole run of six classes: `_PromotePanel`, `_PromotePickerRow`, `_ConfigureMenu`, `_PromotePreparing`, `_PromotePreviewPanel`, `_PromotePushPanel`. The next surviving line is `class _ProjectBar extends StatelessWidget {`.

- [ ] **Step 2: Delete the panel's call site**

In `_ProjectView.build`, delete this line from the `Column`'s `children`:

```dart
                if (envs.length >= 2) _PromotePanel(state: state, item: item, envs: envs),
```

- [ ] **Step 3: Delete the default-direction effect**

Still in `_ProjectView.build`, delete the whole comment block and `if` that seeds the first two envs — from the comment beginning `// First time a ≥2-env project's Promote panel appears` through the closing `}` of:

```dart
    if (envs.length >= 2 && state.promoteSrc == null && state.promoteTgt == null) {
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (state.promoteSrc == null && state.promoteTgt == null) {
          state.setPromoteDir(envs[0].name, envs[1].name);
        }
      });
    }
```

`final envs = item.summary.envs;` immediately above it stays — `_EnvTable` still uses it.

- [ ] **Step 4: Verify it analyzes and the non-promote tests still pass**

Run:
```bash
cd desktop && flutter analyze lib/src/home_page.dart
```
Expected: `No issues found!`

Run:
```bash
cd desktop && flutter test test/project_view_test.dart test/app_state_test.dart test/sidebar_multi_env_test.dart
```
Expected: all pass. (`test/promote_test.dart` is expected to still pass too — it drives `AppState`, not the widgets. It is deleted in Task 2.)

- [ ] **Step 5: Commit**

```bash
git add desktop/lib/src/home_page.dart
git commit -m "refactor(desktop): drop the Promote panel from the Project view

Organization releases go through the GitLab CI deploy job, which runs the
same migrate+sync pair gated on pytest. The panel ran it ungated.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Delete promote state from AppState

**Files:**
- Modify: `desktop/lib/src/app_state.dart`
- Delete: `desktop/test/promote_test.dart`
- Delete: `desktop/test/promote_defaults_test.dart`

**Interfaces:**
- Consumes: Task 1's UI deletion (nothing references these members any more).
- Produces: `AppState` with no `promote*` members, no `_pushSub`, no `revealMapping` / `revealOverlay`. `Settings.promoteDefaults` is now written by nobody and read by nobody; Task 3 removes the field.

- [ ] **Step 1: Delete the two promote test files**

```bash
git rm desktop/test/promote_test.dart desktop/test/promote_defaults_test.dart
```

These test only removed behaviour. There is nothing in them to preserve: `promote_test.dart` covers the stage machine and the project-switch guard, `promote_defaults_test.dart` covers `savePromoteDefaults`/`restorePromoteDefaults` round-tripping.

- [ ] **Step 2: Delete the enum and the state block**

Delete the `PromoteStage` enum together with its doc comment:

```dart
/// Stage of the promote flow for the currently-selected project (there is
/// only ever one promote in flight per project, so this is a single set of
/// fields on [AppState] rather than something keyed like [syncState]).
enum PromoteStage { idle, preparing, preview, pushing, done, error }
```

Then, inside `AppState`, delete the whole block introduced by

```dart
  // ---- promote (migrate src -> tgt, offline preview, then a gated push) ----
```

down to and including the `StreamSubscription<SyncPhase>? _pushSub;` declaration and its doc comment. `import 'dart:async';` becomes unused once `_pushSub` is gone — delete that import too.

- [ ] **Step 3: Delete the methods**

Delete these members entirely, each with its doc comment: `setPromoteDir`, `swapPromoteDir`, `setPromoteMirror`, `setPromotePolicy`, `setPromoteAllowDeletes`, `restorePromoteDefaults`, `savePromoteDefaults`, `resetPromote`, `preparePromote`, `applyPromotePreview`, `pushPromote`, `revealMapping`, `revealOverlay`.

`revealMapping` and `revealOverlay` go because the ⚙ menu was their only caller; `Reveal` on the project still opens the folder, and `.rdc/` is one keystroke away from there. `Future<void> reveal(String folder)` stays — `Reveal` uses it.

- [ ] **Step 4: Clean up the three call sites left behind**

In `selectProject`, the body becomes:

```dart
  void selectProject(String folder) {
    selectedFolder = folder;
    selectedEnv = null; // show the Project view; env children are selected explicitly
    notifyListeners();
  }
```

In `selectEnv`, delete the `if (folderChanged) resetPromote(clearDirection: true);` line and the now-unused `folderChanged` local:

```dart
  void selectEnv(String folder, String env) {
    selectedFolder = folder;
    selectedEnv = env;
    notifyListeners();
  }
```

In `dispose`, delete the `_pushSub?.cancel();` line, leaving:

```dart
  @override
  void dispose() {
    super.dispose();
  }
```

If `dispose` now only calls `super.dispose()`, delete the override entirely — `ChangeNotifier` already does that.

Also check for a leftover `import 'dart:io';` — `revealOverlay` used `Directory`/`Platform` and `revealMapping` used `File`. If nothing else in the file uses `dart:io`, delete the import; `flutter analyze` will tell you.

- [ ] **Step 5: Verify**

Run:
```bash
cd desktop && flutter analyze
```
Expected: `No issues found!` — in particular no "unused import" and no "undefined getter promoteSrc".

Run:
```bash
cd desktop && flutter test
```
Expected: all pass except `test/golden_mdh_test.dart`'s `project view — light`, which is expected to FAIL with a pixel mismatch — the Promote panel is gone from the render. Task 5 regenerates it. Do not regenerate it here; leave the failure visible so Task 5 has something to prove.

- [ ] **Step 6: Commit**

```bash
git add desktop/lib/src/app_state.dart desktop/test/promote_test.dart desktop/test/promote_defaults_test.dart
git commit -m "refactor(desktop): drop promote state, its tests, and the mapping/overlay reveals

The two reveal helpers existed only for the panel's configure menu; Reveal
on the project still opens the folder that contains .rdc/.

The project-view golden fails from here until the panel is re-shot.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Drop `promoteDefaults` from Settings, and start preserving unknown keys

**Files:**
- Modify: `desktop/lib/src/settings.dart`
- Create: `desktop/test/settings_forward_compat_test.dart`

**Interfaces:**
- Consumes: Task 2 (nothing writes `promoteDefaults` any more).
- Produces: `Settings({String? parentFolder, List<String>? externalPaths, Map<String, dynamic>? extra, File? file})`, `Settings.fromJson(Map<String, dynamic>)`, `Settings.toJson()`. The `extra` map holds every key `fromJson` did not recognise and `toJson` re-emits them.

Why: `toJson` currently rebuilds the file from three known keys, so the moment this build saves, an older build's `promoteDefaults` is destroyed. Preserving unknown keys makes a downgrade lossless, and makes every future settings key forward-compatible for free.

- [ ] **Step 1: Write the failing test**

Create `desktop/test/settings_forward_compat_test.dart`:

```dart
import 'package:desktop/src/settings.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('unknown keys survive a load/save round-trip', () {
    // A settings file written by a build that still had promote, plus a
    // hypothetical future key. Neither is known to this build.
    final s = Settings.fromJson({
      'parentFolder': '/tmp/acme',
      'externalPaths': ['/tmp/beta'],
      'promoteDefaults': {
        '/tmp/acme': {'src': 'dev', 'tgt': 'prod', 'mirror': true, 'policy': 'keepTarget'},
      },
      'somethingFromTheFuture': 42,
    });

    expect(s.parentFolder, '/tmp/acme');
    expect(s.externalPaths, ['/tmp/beta']);

    final out = s.toJson();
    expect(out['promoteDefaults'], isNotNull,
        reason: 'an older build must still find its promote defaults after this build saves');
    expect((out['promoteDefaults'] as Map)['/tmp/acme']['src'], 'dev');
    expect(out['somethingFromTheFuture'], 42);
  });

  test('a known key is never shadowed by the preserved map', () {
    final s = Settings.fromJson({'parentFolder': '/tmp/old'});
    s.parentFolder = '/tmp/new';
    expect(s.toJson()['parentFolder'], '/tmp/new');
  });
}
```

- [ ] **Step 2: Run it and watch it fail**

Run:
```bash
cd desktop && flutter test test/settings_forward_compat_test.dart
```
Expected: FAIL on the first test — `out['promoteDefaults']` is `null`, because today's `toJson` emits exactly three hardcoded keys.

- [ ] **Step 3: Implement**

In `desktop/lib/src/settings.dart`, replace the `promoteDefaults` field, its constructor parameter, and the `fromJson`/`toJson` bodies:

```dart
  String? parentFolder;
  List<String> externalPaths;

  /// Every key in the on-disk JSON this build does not recognise, kept
  /// verbatim so a save never destroys them. That covers `promoteDefaults`
  /// (written by builds that still had the Promote panel — downgrading must
  /// still find it) and any key a future build adds.
  final Map<String, dynamic> extra;

  /// Keys this build owns. Anything else lands in [extra].
  static const _known = {'parentFolder', 'externalPaths'};

  final File? _overrideFile;

  Settings({
    this.parentFolder,
    List<String>? externalPaths,
    Map<String, dynamic>? extra,
    File? file,
  })  : externalPaths = externalPaths ?? [],
        extra = extra ?? {},
        _overrideFile = file;
```

```dart
  static Settings fromJson(Map<String, dynamic> m) => Settings(
        parentFolder: m['parentFolder'] as String?,
        externalPaths:
            (m['externalPaths'] as List?)?.map((e) => e as String).toList() ?? [],
        extra: {
          for (final e in m.entries)
            if (!_known.contains(e.key)) e.key: e.value,
        },
      );

  /// Known keys are written last so a stale value in [extra] can never
  /// shadow the live one.
  Map<String, dynamic> toJson() => {
        ...extra,
        'parentFolder': parentFolder,
        'externalPaths': externalPaths,
      };
```

- [ ] **Step 4: Run the test**

Run:
```bash
cd desktop && flutter test test/settings_forward_compat_test.dart
```
Expected: both tests PASS.

- [ ] **Step 5: Verify nothing else referenced the field**

Run:
```bash
cd desktop && grep -rn "promoteDefaults" lib test ; flutter analyze
```
Expected: `grep` prints nothing outside the new test's fixture JSON; `flutter analyze` prints `No issues found!`.

- [ ] **Step 6: Commit**

```bash
git add desktop/lib/src/settings.dart desktop/test/settings_forward_compat_test.dart
git commit -m "feat(desktop): preserve unknown settings keys; drop promoteDefaults

toJson rebuilt the file from its known keys, so this build's first save
would have destroyed an older build's promote defaults. Unknown keys are
now round-tripped verbatim, which also makes every future key forward
compatible.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Remove promote from the bridge crate and regenerate the bindings

**Files:**
- Modify: `desktop/rust/src/api/rdc.rs`
- Regenerate: `desktop/lib/src/rust/api/rdc.dart`, `desktop/lib/src/rust/api/rdc.freezed.dart`, `desktop/lib/src/rust/frb_generated.dart`, `desktop/lib/src/rust/frb_generated.io.dart`, `desktop/lib/src/rust/frb_generated.web.dart`, `desktop/rust/src/frb_generated.rs`

**Interfaces:**
- Consumes: Tasks 1–3 (no Dart caller remains).
- Produces: the bridge's exported surface is `init_app`, `rdc_version`, `list_projects`, `add_project`, `validate_existing_project`, `edit_project`, `add_env`, `remove_env`, `rename_env`, `sync_env`, `trash_project`, `reveal_in_file_manager`. `SyncPhase` is unchanged (`Started` / `Log` / `Done` / `Error`). `ConflictPolicy` and `PromotionPreview` no longer exist in Dart.

- [ ] **Step 1: Delete the promote section from the bridge**

In `desktop/rust/src/api/rdc.rs`, delete everything from

```rust
// ---------------------------------------------------------------- promote
```

down to (but not including)

```rust
// ---------------------------------------------------------------- trash / reveal
```

That removes, in order: `ConflictPolicy`, `policy_to_strategy`, `PromotionPreview`, `prepare_promotion`, `push_promotion`, `env_api_base`, and `LineCollector` with its `impl std::io::Write`.

Keep `LineForwarder` and its `impl std::io::Write` — they live in the **sync** section above and `sync_env` uses them.

- [ ] **Step 2: Delete the promote unit test**

In the same file's `mod tests`, delete:

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

- [ ] **Step 3: Fix the imports**

At the top of the file, `use rdc::cli::sync::CycleOutcome;` was used only by `push_promotion`'s `let result: Result<CycleOutcome>`, and `use std::sync::{Arc, Mutex};` only by `LineCollector`. `use std::path::{Path, PathBuf};` loses `Path` when `env_api_base` goes. Delete whatever the compiler reports as unused; do not guess — Step 4 tells you.

- [ ] **Step 4: Build the bridge crate**

Run:
```bash
cd desktop/rust && cargo build 2>&1 | tail -30
```
Expected: compiles. Any `unused import` warning names exactly what Step 3 missed — remove it and rebuild until the output is clean.

- [ ] **Step 5: Run the bridge crate's tests**

Run:
```bash
cd desktop/rust && cargo test
```
Expected: all pass. The count drops by exactly one (the deleted `conflict_policy_maps_inverted_for_promote_push`).

- [ ] **Step 6: Confirm the codegen version, then regenerate**

Run:
```bash
flutter_rust_bridge_codegen --version
```
Expected: `flutter_rust_bridge_codegen 2.12.0`. **If it prints anything else, stop** — the generated files are committed and a different version rewrites them wholesale. Install the pin with `cargo install flutter_rust_bridge_codegen --version 2.12.0`.

Run:
```bash
cd desktop && flutter_rust_bridge_codegen generate
```

- [ ] **Step 7: Check the regeneration did what you expect**

Run:
```bash
cd /Users/martin.zlamal@rossum.ai/Work/github.com/mrtnzlml/rdc && git diff --stat desktop/lib/src/rust desktop/rust/src/frb_generated.rs
git diff desktop/lib/src/rust/api/rdc.dart | grep '^-' | grep -i -c 'promot\|conflictpolicy'
```
Expected: the six generated files change; the second command prints a non-zero count, and `git diff` contains **no** additions mentioning promote. If the diff shows unrelated churn across the whole file, the codegen version is wrong — revert with `git checkout -- desktop/lib/src/rust desktop/rust/src/frb_generated.rs` and redo Step 6.

- [ ] **Step 8: Verify the app still analyzes and builds**

Run:
```bash
cd desktop && flutter analyze && flutter test test/app_state_test.dart
```
Expected: `No issues found!` and the test passes.

- [ ] **Step 9: Commit**

```bash
git add desktop/rust/src/api/rdc.rs desktop/lib/src/rust desktop/rust/src/frb_generated.rs
git commit -m "refactor(desktop): remove promote from the bridge and regenerate bindings

Drops prepare_promotion, push_promotion, ConflictPolicy, PromotionPreview,
LineCollector and env_api_base, and with them the app's last call into
rdc::cli::migrate. The app can no longer write to any environment other
than the one being synced.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Re-shoot the project-view golden and update the README

**Files:**
- Modify: `desktop/test/goldens/mdh_project_light.png` (regenerated)
- Modify: `desktop/README.md`

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces: a green `flutter test`, and a README that describes the app as it now behaves.

- [ ] **Step 1: Confirm the golden is failing for the right reason**

Run:
```bash
cd desktop && flutter test test/golden_mdh_test.dart 2>&1 | tail -20
```
Expected: `project view — light` FAILS with a pixel diff; every other golden passes. If any *other* golden also fails, stop and investigate — the Promote panel only appeared in the project view, so nothing else should have moved.

- [ ] **Step 2: Regenerate only that golden**

Run:
```bash
cd desktop && flutter test --update-goldens test/golden_mdh_test.dart
```

- [ ] **Step 3: Eyeball the new image**

Run:
```bash
cd /Users/martin.zlamal@rossum.ai/Work/github.com/mrtnzlml/rdc && git diff --stat desktop/test/goldens/
```
Expected: **only** `mdh_project_light.png` changed. If another `.png` shows up in the diff, revert it (`git checkout -- desktop/test/goldens/<name>.png`) and find out why it moved before continuing.

Open `desktop/test/goldens/mdh_project_light.png` and confirm: the Environments table is there, the Promote card below it is gone, and nothing is clipped or left with a dangling section title.

- [ ] **Step 4: Run the whole Dart suite**

Run:
```bash
cd desktop && flutter test
```
Expected: everything passes.

- [ ] **Step 5: Update the README**

In `desktop/README.md`, delete the entire `## Promote` section — from the `## Promote` heading through the end of the **Known limitation** paragraph, i.e. everything before `## Prerequisites`.

In the `## Layout` block, change the bridge-surface comment so the op count is not a lie:

```
    src/api/rdc.rs     the FRB-exposed surface (delegates to `rdc`)
```

- [ ] **Step 6: Check no other doc still promises Promote**

Run:
```bash
cd /Users/martin.zlamal@rossum.ai/Work/github.com/mrtnzlml/rdc && grep -rn -i "promote" desktop/README.md README.md desktop/lib desktop/rust/src desktop/test
```
Expected: no hits, except the fixture JSON key inside `desktop/test/settings_forward_compat_test.dart` (that one is deliberate — it is the downgrade case being tested).

- [ ] **Step 7: Commit**

```bash
git add desktop/test/goldens/mdh_project_light.png desktop/README.md
git commit -m "docs(desktop): re-shoot the project golden and drop the Promote section

The Known limitation about Prepare overwriting the target snapshot goes
with it — it is no longer true of anything the app does.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Done criteria

All of these must hold before the plan is finished:

```bash
cd desktop && flutter analyze          # No issues found!
cd desktop && flutter test             # all green, goldens included
cd desktop/rust && cargo test          # all green
cd desktop && flutter build macos      # bundle builds
```

And: `grep -rn -i promote desktop/lib desktop/rust/src` returns nothing.
