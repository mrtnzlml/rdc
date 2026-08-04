import 'dart:async';
import 'dart:io';

import 'package:flutter/foundation.dart';

import 'error_text.dart';
import 'rust/api/rdc.dart';
import 'settings.dart';

enum SyncState { idle, running, done, error }

/// Stage of the promote flow for the currently-selected project (there is
/// only ever one promote in flight per project, so this is a single set of
/// fields on [AppState] rather than something keyed like [syncState]).
enum PromoteStage { idle, preparing, preview, pushing, done, error }

/// A discovered project plus whether it lives outside the parent folder (an
/// "external" project attached via Open Existing).
class ProjectItem {
  final ProjectSummary summary;
  final bool isExternal;
  const ProjectItem(this.summary, this.isExternal);
}

/// Single source of truth for the UI. Wraps the Rust bridge and derives the
/// project list from disk (parent scan ∪ attached externals). Transient sync
/// state is keyed per (folder, env) via [envKey].
class AppState extends ChangeNotifier {
  AppState(this._settings);

  final Settings _settings;

  List<ProjectItem> projects = [];
  String? selectedFolder;
  String? selectedEnv;
  bool loading = false;
  String? lastError;

  final Map<String, SyncState> syncState = {};
  final Map<String, String> syncMessage = {};
  final Map<String, List<String>> syncLog = {};

  // ---- promote (migrate src -> tgt, offline preview, then a gated push) ----
  String? promoteSrc;
  String? promoteTgt;
  bool promoteMirror = false;
  ConflictPolicy promotePolicy = ConflictPolicy.keepTarget;
  bool promoteAllowDeletes = false;
  PromotionPreview? promotePreview;
  List<String> promoteLog = [];
  PromoteStage promoteStage = PromoteStage.idle;
  String? promoteError;

  /// The live subscription behind an in-flight [pushPromote], if any. Kept
  /// so [resetPromote] (and a project switch, which calls it) can cancel a
  /// still-running push instead of leaving it silently writing into fields
  /// that may since belong to a different project's Promote panel.
  StreamSubscription<SyncPhase>? _pushSub;

  String? get parentFolder => _settings.parentFolder;

  String envKey(String folder, String env) => '$folder\u0000$env';

  ProjectItem? get selected {
    final folder = selectedFolder;
    if (folder == null) return null;
    for (final p in projects) {
      if (p.summary.folder == folder) return p;
    }
    return null;
  }

  EnvSummary? get selectedEnvSummary {
    final p = selected;
    final env = selectedEnv;
    if (p == null || env == null) return null;
    for (final e in p.summary.envs) {
      if (e.name == env) return e;
    }
    return null;
  }

  void selectProject(String folder) {
    final folderChanged = folder != selectedFolder;
    if (folderChanged) {
      resetPromote(clearDirection: true);
      restorePromoteDefaults(folder);
    }
    selectedFolder = folder;
    selectedEnv = null; // show the Project view; env children are selected explicitly
    notifyListeners();
  }

  void selectEnv(String folder, String env) {
    final folderChanged = folder != selectedFolder;
    if (folderChanged) resetPromote(clearDirection: true);
    selectedFolder = folder;
    selectedEnv = env;
    notifyListeners();
  }

  Future<void> setParentFolder(String path) async {
    _settings.parentFolder = path;
    _settings.save();
    await reload();
  }

  Future<void> reload() async {
    loading = true;
    notifyListeners();

    final byFolder = <String, ProjectItem>{};
    try {
      final parent = _settings.parentFolder;
      if (parent != null) {
        for (final s in await listProjects(parent: parent)) {
          byFolder[s.folder] = ProjectItem(s, false);
        }
      }
      final stillValid = <String>[];
      for (final path in List<String>.from(_settings.externalPaths)) {
        try {
          final s = await validateExistingProject(path: path);
          byFolder.putIfAbsent(s.folder, () => ProjectItem(s, true));
          stillValid.add(path);
        } catch (_) {}
      }
      if (stillValid.length != _settings.externalPaths.length) {
        _settings.externalPaths = stillValid;
        _settings.save();
      }
    } catch (e) {
      lastError = errorText(e);
    }

    projects = byFolder.values.toList()
      ..sort((a, b) => a.summary.name.toLowerCase().compareTo(b.summary.name.toLowerCase()));
    if (selectedFolder != null && !projects.any((p) => p.summary.folder == selectedFolder)) {
      selectedFolder = null;
      selectedEnv = null;
    }
    // Re-pin the selected env if it vanished (e.g. removed on disk). Only
    // applies when an env was actually selected — `selectedEnv == null` means
    // the Project view is showing on purpose and must not be disturbed.
    if (selectedFolder != null && selectedEnv != null && selected != null &&
        !selected!.summary.envs.any((e) => e.name == selectedEnv)) {
      selectedEnv = selected!.summary.envs.isNotEmpty ? selected!.summary.envs.first.name : null;
    }
    if (selectedFolder == null && projects.isNotEmpty) {
      selectProject(projects.first.summary.folder);
    }
    loading = false;
    notifyListeners();
  }

  Future<void> addProjectEntry(String projectName, AddEnvInput firstEnv) async {
    final parent = _settings.parentFolder;
    if (parent == null) throw Exception('Choose a parent folder first.');
    final s = await addProject(parent: parent, projectName: projectName, firstEnv: firstEnv);
    await reload();
    selectProject(s.folder);
  }

  /// Whether `env` under `folder` is safe to rename right now — refused while
  /// a sync is actively running against it (a rename mid-sync would move the
  /// very paths the in-flight sync is reading/writing out from under it).
  bool canRenameEnv(String folder, String env) => syncState[envKey(folder, env)] != SyncState.running;

  /// Edit `env`'s connection details, optionally renaming it first.
  ///
  /// When `newEnvName` names a different env than `env.name`, the rename is
  /// guarded by [canRenameEnv] (refused mid-sync) and applied via `renameEnv`
  /// before the subsequent `editProject` — which then targets the *new* name,
  /// since `renameEnv` has already moved the env's on-disk section/paths.
  ///
  /// `renameEnv` commits to disk immediately, so if the following
  /// `editProject`/reselect then throws, [projects] must still be refreshed
  /// to reflect the completed rename — otherwise it keeps showing the OLD
  /// name and a retry from the still-open dialog would re-send it, hitting
  /// "This project has no `<old>` environment" and masking that the rename
  /// took. Not needed on the non-rename path: nothing moved on disk there,
  /// so the pre-existing state is still accurate on failure.
  Future<void> editEnvEntry(
    ProjectItem item,
    EnvSummary env,
    EditConnectionInput input, {
    String? newEnvName,
  }) async {
    var targetEnv = env.name;
    final renamed = newEnvName != null && newEnvName != env.name;
    if (renamed) {
      if (!canRenameEnv(item.summary.folder, env.name)) {
        throw Exception("Can't rename while this environment is syncing.");
      }
      await renameEnv(folder: item.summary.folder, old: env.name, new_: newEnvName);
      targetEnv = newEnvName;
    }
    try {
      final updated = await editProject(folder: item.summary.folder, env: targetEnv, input: input);
      if (item.isExternal && updated.folder != item.summary.folder) {
        final i = _settings.externalPaths.indexOf(item.summary.folder);
        if (i >= 0) {
          _settings.externalPaths[i] = updated.folder;
          _settings.save();
        }
      }
      await reload();
      selectEnv(updated.folder, targetEnv);
    } catch (e) {
      if (renamed) await reload();
      rethrow;
    }
  }

  Future<void> addEnvEntry(ProjectItem item, AddEnvInput input) async {
    await addEnv(folder: item.summary.folder, input: input);
    await reload();
    selectEnv(item.summary.folder, input.name.trim());
  }

  Future<void> removeEnvEntry(ProjectItem item, EnvSummary env) async {
    final updated = await removeEnv(folder: item.summary.folder, env: env.name);
    await reload();
    if (updated == null) {
      // whole project was removed (last env)
      if (selectedFolder == item.summary.folder) {
        selectedFolder = null;
        selectedEnv = null;
      }
    } else {
      selectProject(item.summary.folder); // back to the Project view
    }
  }

  Future<void> openExisting(String path) async {
    final s = await validateExistingProject(path: path);
    final parent = _settings.parentFolder;
    final underParent = parent != null && s.folder.startsWith(parent);
    if (!underParent && !_settings.externalPaths.contains(s.folder)) {
      _settings.externalPaths.add(s.folder);
      _settings.save();
    }
    await reload();
    selectProject(s.folder);
  }

  Future<void> removeOrDetach(ProjectItem item) async {
    if (item.isExternal) {
      _settings.externalPaths.remove(item.summary.folder);
      _settings.save();
    } else {
      await trashProject(folder: item.summary.folder);
    }
    if (selectedFolder == item.summary.folder) {
      selectedFolder = null;
      selectedEnv = null;
    }
    await reload();
  }

  Future<void> reveal(String folder) => revealInFileManager(path: folder);

  /// Reveal the N-way mapping file (`.rdc/mapping.toml`) that records this
  /// project's per-env slug divergences, or the project folder itself if it
  /// doesn't exist yet (e.g. nothing has diverged, so rdc hasn't written it).
  Future<void> revealMapping(ProjectItem p) async {
    final folder = p.summary.folder;
    final sep = Platform.pathSeparator;
    final mapping = File('$folder$sep.rdc${sep}mapping.toml');
    await reveal(mapping.existsSync() ? mapping.path : folder);
  }

  /// Reveal `<folder>/envs/<tgt>/overlay/`, creating it first if it doesn't
  /// exist yet — this is where target-only attribute overrides for a promote
  /// live, and a fresh env may not have the directory on disk at all.
  Future<void> revealOverlay(ProjectItem p, String tgt) async {
    final sep = Platform.pathSeparator;
    final dir = Directory('${p.summary.folder}${sep}envs$sep$tgt${sep}overlay');
    dir.createSync(recursive: true);
    await reveal(dir.path);
  }

  void syncEnvItem(ProjectItem item, EnvSummary env) {
    final folder = item.summary.folder;
    final k = envKey(folder, env.name);
    syncState[k] = SyncState.running;
    syncMessage.remove(k);
    syncLog[k] = <String>[];
    notifyListeners();

    syncEnv(folder: folder, env: env.name, apiBase: env.apiBase, orgId: env.orgId).listen(
      (phase) {
        switch (phase) {
          case SyncPhase_Started():
            syncState[k] = SyncState.running;
          case SyncPhase_Log(:final line):
            (syncLog[k] ??= <String>[]).add(line);
          case SyncPhase_Done(:final fileCount):
            syncState[k] = SyncState.done;
            syncMessage[k] = 'Pulled $fileCount files';
            reload();
          case SyncPhase_Error(:final message):
            syncState[k] = SyncState.error;
            syncMessage[k] = message;
        }
        notifyListeners();
      },
      onError: (Object e) {
        syncState[k] = SyncState.error;
        syncMessage[k] = errorText(e);
        notifyListeners();
      },
    );
  }

  /// Pick (or change) the promote direction. Setting both to `null` clears
  /// the picker back to unselected (used when the project changes).
  void setPromoteDir(String? src, String? tgt) {
    promoteSrc = src;
    promoteTgt = tgt;
    notifyListeners();
  }

  void swapPromoteDir() {
    final src = promoteSrc;
    promoteSrc = promoteTgt;
    promoteTgt = src;
    notifyListeners();
  }

  void setPromoteMirror(bool value) {
    promoteMirror = value;
    notifyListeners();
  }

  void setPromotePolicy(ConflictPolicy value) {
    promotePolicy = value;
    notifyListeners();
  }

  void setPromoteAllowDeletes(bool value) {
    promoteAllowDeletes = value;
    notifyListeners();
  }

  /// Re-arm `promoteSrc`/`promoteTgt`/`promoteMirror`/`promotePolicy` from
  /// the last direction saved for `folder` (see [savePromoteDefaults]).
  /// No-op if this project has never had a promote prepared — the
  /// first-two-envs default in `_ProjectView` then takes over.
  ///
  /// A saved `src`/`tgt` is only restored if it still names an env that
  /// exists on this project *today* — otherwise a since-renamed/removed env
  /// would come back as a stale selection (blank dropdown, but a `Prepare`
  /// button that could still fire against the bridge with the dead name).
  /// Guarded end-to-end in a try/catch: a hand-edited/corrupted settings
  /// value (wrong types, etc.) must not throw inside `selectProject`.
  void restorePromoteDefaults(String folder) {
    try {
      final d = _settings.promoteDefaults[folder];
      if (d == null) return;
      ProjectItem? project;
      for (final p in projects) {
        if (p.summary.folder == folder) {
          project = p;
          break;
        }
      }
      final envNames = project?.summary.envs.map((e) => e.name).toSet() ?? <String>{};
      final savedSrc = d['src'] as String?;
      final savedTgt = d['tgt'] as String?;
      promoteSrc = (savedSrc != null && envNames.contains(savedSrc)) ? savedSrc : null;
      promoteTgt = (savedTgt != null && envNames.contains(savedTgt)) ? savedTgt : null;
      promoteMirror = d['mirror'] as bool? ?? false;
      final policyName = d['policy'] as String?;
      for (final p in ConflictPolicy.values) {
        if (p.name == policyName) {
          promotePolicy = p;
          break;
        }
      }
    } catch (_) {
      // Corrupted/hand-edited settings must not crash selectProject — fall
      // back to no restored direction (the picker's own default kicks in).
      promoteSrc = null;
      promoteTgt = null;
    }
    notifyListeners();
  }

  /// Persist the current promote direction/mirror/policy for `folder` so
  /// re-selecting this project later re-arms the same picks (see
  /// [restorePromoteDefaults]).
  void savePromoteDefaults(String folder) {
    _settings.promoteDefaults[folder] = {
      'src': promoteSrc,
      'tgt': promoteTgt,
      'mirror': promoteMirror,
      'policy': promotePolicy.name,
    };
    _settings.save();
  }

  /// Abandon the current preview/push and go back to idle. By default the
  /// picked direction, mirror flag, policy, and allow-deletes are left alone
  /// so a Cancel doesn't force the user to redo their picks before
  /// re-Preparing.
  ///
  /// Pass `clearDirection: true` when the *project* itself is changing (see
  /// [selectProject]/[selectEnv]) — a promote in flight belongs to one
  /// project, and leaving `promoteSrc`/`promoteTgt` (or a stale
  /// `promotePreview`) set after switching projects would let the Promote
  /// panel render another project's plan against the newly-selected one —
  /// including a live "Push" button wired to push it. Clearing the picked
  /// envs also re-arms the Project view's default-direction effect for the
  /// new project's env list.
  void resetPromote({bool clearDirection = false}) {
    _pushSub?.cancel();
    _pushSub = null;
    promoteStage = PromoteStage.idle;
    promotePreview = null;
    promoteLog = [];
    promoteError = null;
    if (clearDirection) {
      promoteSrc = null;
      promoteTgt = null;
    }
    notifyListeners();
  }

  /// Offline dry-run: migrate `src` -> `tgt` locally and capture the rendered
  /// push plan, without touching the target remote.
  ///
  /// This awaits the bridge, so the user is free to switch to a different
  /// project while it's in flight. If they do, `p`'s eventual result must
  /// not land in the *new* project's Promote panel (which would render a
  /// stranger's reviewed plan under this project's live "Push" button) — see
  /// [applyPrepareResult] and the matching guard in the catch branch below.
  Future<void> preparePromote(ProjectItem p) async {
    final src = promoteSrc;
    final tgt = promoteTgt;
    if (src == null || tgt == null || src == tgt) return;
    final folder = p.summary.folder;
    savePromoteDefaults(folder); // remember the direction actually prepared
    promoteStage = PromoteStage.preparing;
    promoteError = null;
    notifyListeners();

    try {
      final preview = await preparePromotion(
        folder: folder,
        src: src,
        tgt: tgt,
        mirror: promoteMirror,
      );
      applyPrepareResult(folder, preview);
    } catch (e) {
      if (selectedFolder != folder) return; // navigated away; B's panel must not show A's error
      promoteStage = PromoteStage.error;
      promoteError = errorText(e);
      notifyListeners();
    }
  }

  /// Applies a resolved [preparePromote] result for `folder` — a no-op if
  /// `folder` is no longer the selected project (the user switched away
  /// while the prepare was in flight). Split out from [preparePromote] so
  /// the guard is unit-testable without an actual bridge round-trip.
  @visibleForTesting
  void applyPrepareResult(String folder, PromotionPreview preview) {
    if (selectedFolder != folder) return;
    promotePreview = preview;
    promoteStage = PromoteStage.preview;
    notifyListeners();
  }

  /// The real gated push, streaming rdc's log. Modeled on [syncEnvItem].
  ///
  /// Like [preparePromote], this keeps running across a project switch (it's
  /// a stream, not a single await), so every phase callback re-checks that
  /// `folder` is still selected before touching `promoteStage`/`promoteLog` —
  /// otherwise a switch mid-push would leave this project's log streaming
  /// into whatever project the user switched to, under its live Push button.
  void pushPromote(ProjectItem p) {
    final src = promoteSrc;
    final tgt = promoteTgt;
    if (src == null || tgt == null || src == tgt) return;
    final folder = p.summary.folder;
    promoteStage = PromoteStage.pushing;
    promoteLog = <String>[];
    promoteError = null;
    notifyListeners();

    _pushSub?.cancel();
    _pushSub = pushPromotion(
      folder: folder,
      src: src,
      tgt: tgt,
      mirror: promoteMirror,
      policy: promotePolicy,
      allowDeletes: promoteAllowDeletes,
    ).listen(
      (phase) {
        if (selectedFolder != folder) {
          _pushSub?.cancel();
          _pushSub = null;
          return; // navigated away; don't clobber the newly-selected project
        }
        switch (phase) {
          case SyncPhase_Started():
            promoteStage = PromoteStage.pushing;
          case SyncPhase_Log(:final line):
            promoteLog.add(line);
          case SyncPhase_Done():
            promoteStage = PromoteStage.done;
            reload();
          case SyncPhase_Error(:final message):
            promoteStage = PromoteStage.error;
            promoteError = message;
        }
        notifyListeners();
      },
      onError: (Object e) {
        if (selectedFolder != folder) {
          _pushSub?.cancel();
          _pushSub = null;
          return;
        }
        promoteStage = PromoteStage.error;
        promoteError = errorText(e);
        notifyListeners();
      },
    );
  }

  void clearError() {
    lastError = null;
    notifyListeners();
  }

  @override
  void dispose() {
    _pushSub?.cancel();
    super.dispose();
  }
}
