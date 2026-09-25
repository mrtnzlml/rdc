import 'dart:async';

import 'package:flutter/foundation.dart';

import 'error_text.dart';
import 'rust/api/rdc.dart';
import 'settings.dart';
import 'watch_state.dart';

enum SyncState { idle, running, done, error }

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
/// What a rename left for the user: rdc's warnings, and the GitLab work rdc
/// cannot do (CI variables, environment history).
class RenameNotes {
  const RenameNotes({this.warnings = const [], this.followUps = const []});
  final List<String> warnings;
  final List<String> followUps;
  bool get isEmpty => warnings.isEmpty && followUps.isEmpty;
}

/// The snackbar text after a rename, or null when there is nothing to say.
String? renameNotesMessage(RenameNotes notes) {
  if (notes.isEmpty) return null;
  String list(List<String> items) => items.map((n) => '• $n').join('\n');
  return [
    'Renamed.',
    if (notes.followUps.isNotEmpty) 'Still to do:\n${list(notes.followUps)}',
    if (notes.warnings.isNotEmpty) 'Warnings:\n${list(notes.warnings)}',
  ].join('\n');
}

/// A step after a completed rename failed. Carries the rename's notes so the
/// follow-ups still reach the user; the rename itself is not undone.
class RenamedButFailed implements Exception {
  RenamedButFailed(this.cause, this.notes);
  final Object cause;
  final RenameNotes notes;

  @override
  String toString() {
    final message = renameNotesMessage(notes);
    final renamed = message == null
        ? 'The environment was renamed.'
        : 'The environment was renamed. ${message.replaceFirst('Renamed.\n', '')}';
    return '${errorText(cause)}\n\n$renamed';
  }
}

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

  /// Live watches, keyed like [syncState] by (folder, env).
  final Map<String, WatchState> watch = {};

  /// Ticks every watching env's [WatchState.nextPollSecs] down to zero once
  /// a second. `SyncPhase.idle` only ever *sets* the countdown (once per
  /// cycle boundary); without something ticking between those sets, the
  /// sidebar would show a static number instead of a countdown. Started by
  /// [_ensureIdleTicker] when the first watch begins, stopped by
  /// [_stopIdleTickerIfIdle] once [watch] is empty again.
  Timer? _idleTicker;

  void _ensureIdleTicker() {
    _idleTicker ??= Timer.periodic(const Duration(seconds: 1), (_) {
      var changed = false;
      for (final w in watch.values) {
        final secs = w.nextPollSecs;
        if (secs != null && secs > 0) {
          w.nextPollSecs = secs - 1;
          changed = true;
        }
      }
      if (changed) notifyListeners();
    });
  }

  void _stopIdleTickerIfIdle() {
    if (watch.isEmpty) {
      _idleTicker?.cancel();
      _idleTicker = null;
    }
  }

  @override
  void dispose() {
    _idleTicker?.cancel();
    _idleTicker = null;
    super.dispose();
  }

  bool isWatching(String folder, String env) => watch[envKey(folder, env)]?.running ?? false;

  /// Every prompt currently blocking, keyed like [syncState] by (folder, env).
  ///
  /// Deliberately NOT stored inside [WatchState]: a one-shot `Sync` blocks on
  /// exactly the same gates a watch does, and its bridge call installs a real
  /// prompt route. If prompts lived only on watch state, a gate hit during a
  /// plain Sync would emit `SyncPhase.prompt`, find no consumer, and leave the
  /// bridge thread blocked forever with no dialog to answer — strictly worse
  /// than the silent skip it replaced. Both streams write here.
  final Map<String, PendingPrompt> pendingPrompts = {};

  /// Every prompt currently blocking, oldest first. More than one is
  /// reachable: two watched envs, or a watch and a one-shot sync, can block
  /// at the same time.
  List<PendingPrompt> get promptQueue => pendingPrompts.values.toList();

  String? get parentFolder => _settings.parentFolder;

  String envKey(String folder, String env) => '$folder\u0000$env';

  /// True when this project's owner has not yet been told that Sync writes
  /// to Rossum. The app was pull-only (`--no-push`) until this release.
  bool needsTwoWayNotice(String folder) => !_settings.hasAckedTwoWay(folder);

  /// Records that `folder`'s owner has seen the one-time two-way notice.
  void ackTwoWay(String folder) {
    _settings.ackTwoWayFor(folder);
    notifyListeners();
  }

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
    selectedFolder = folder;
    selectedEnv = null; // show the Project view; env children are selected explicitly
    notifyListeners();
  }

  void selectEnv(String folder, String env) {
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
  ///
  /// Returns the rename's warnings and GitLab follow-ups, empty when nothing
  /// was renamed. If a step after a completed rename fails, the error is a
  /// [RenamedButFailed] that still carries them.
  Future<RenameNotes> editEnvEntry(
    ProjectItem item,
    EnvSummary env,
    EditConnectionInput input, {
    String? newEnvName,
  }) async {
    var targetEnv = env.name;
    var notes = const RenameNotes();
    final renamed = newEnvName != null && newEnvName != env.name;
    if (renamed) {
      if (!canRenameEnv(item.summary.folder, env.name)) {
        throw Exception("Can't rename while this environment is syncing.");
      }
      final r = await renameEnv(folder: item.summary.folder, old: env.name, new_: newEnvName);
      notes = RenameNotes(warnings: r.warnings, followUps: r.followUps);
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
      return notes;
    } catch (e) {
      if (!renamed) rethrow;
      await reload();
      throw RenamedButFailed(e, notes);
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
          case SyncPhase_Prompt(:final id, :final kind, :final question, :final keys):
            // A one-shot sync blocks on exactly the same gates a watch does,
            // and its bridge call installs a real prompt route — so this
            // must be answered here too, not left for the watch stream.
            pendingPrompts[k] = PendingPrompt(
              id: id,
              kind: kind,
              question: question,
              keys: keys,
              folder: folder,
              env: env.name,
            );
          case SyncPhase_PromptResolved(:final id):
            resolvePrompt(k, id);
          case SyncPhase_Done(:final fileCount):
            syncState[k] = SyncState.done;
            syncMessage[k] = 'Synced · $fileCount files';
            reload();
          case SyncPhase_Error(:final message):
            syncState[k] = SyncState.error;
            pendingPrompts.remove(k);
            syncMessage[k] = message;
          case SyncPhase_Idle():
          case SyncPhase_Stopped():
            break; // a one-shot sync never emits these
        }
        notifyListeners();
      },
      onError: (Object e) {
        syncState[k] = SyncState.error;
        pendingPrompts.remove(k);
        syncMessage[k] = errorText(e);
        notifyListeners();
      },
    );
  }

  /// Clear the pending prompt for `k`, but only if it is the one being
  /// resolved. A watch and a displaced predecessor can both be unwinding at
  /// once, so a late resolve must not clear a newer prompt.
  void resolvePrompt(String k, BigInt id) {
    if (pendingPrompts[k]?.id == id) pendingPrompts.remove(k);
  }

  void watchEnvItem(ProjectItem item, EnvSummary env) {
    final folder = item.summary.folder;
    final k = envKey(folder, env.name);
    if (watch[k]?.running ?? false) return; // already watching
    watch[k] = WatchState(running: true);
    syncLog[k] = <String>[];
    notifyListeners();

    watchEnv(
      folder: folder,
      env: env.name,
      apiBase: env.apiBase,
      orgId: env.orgId,
      pollSecs: BigInt.from(_settings.pollSecsFor(folder, env.name) ?? 60),
    ).listen(
      (phase) => applyWatchPhase(folder, env.name, phase),
      onError: (Object e) {
        watch.remove(k);
        _stopIdleTickerIfIdle();
        syncState[k] = SyncState.error;
        pendingPrompts.remove(k);
        syncMessage[k] = errorText(e);
        notifyListeners();
      },
    );
  }

  /// Applies one phase of a watch's stream to state. Split out of
  /// [watchEnvItem]'s `listen` callback so this logic — the terminal `Error`
  /// handling in particular — can be driven directly by a test: the stream
  /// itself comes from a live Rust bridge call and can't run under
  /// `flutter test`.
  ///
  /// A no-op if the watch was already stopped and cleared while this phase
  /// was in flight (`watch[k]` gone).
  void applyWatchPhase(String folder, String envName, SyncPhase phase) {
    final k = envKey(folder, envName);
    final w = watch[k];
    if (w == null) return;
    switch (phase) {
      case SyncPhase_Started():
        w.running = true;
        _ensureIdleTicker(); // starts on the first watch; a no-op if already running
      case SyncPhase_Log(:final line):
        (syncLog[k] ??= <String>[]).add(line);
        w.nextPollSecs = null; // a cycle is running
      case SyncPhase_Prompt(:final id, :final kind, :final question, :final keys):
        pendingPrompts[k] = PendingPrompt(
          id: id,
          kind: kind,
          question: question,
          keys: keys,
          folder: folder,
          env: envName,
        );
      case SyncPhase_PromptResolved(:final id):
        resolvePrompt(k, id);
      case SyncPhase_Idle(:final nextPollSecs):
        w.nextPollSecs = nextPollSecs?.toInt();
      case SyncPhase_Done():
        reload();
      case SyncPhase_Error(:final message):
        // Terminal: the Rust side makes Error and Stopped mutually
        // exclusive outcomes of the same watch call, so once an Error
        // phase has arrived no Stopped is ever coming to clear this entry.
        // Leaving it in `watch` would permanently block Sync (any
        // presence-based "a watch still owns this env" check reads true
        // forever) and leave Watch stuck disabled, with no way to reach
        // "Stop" — the env's controls would need an app restart to
        // recover, for something as ordinary as a network blip or an
        // expired token. `onError` below already clears the entry for a
        // broken *stream*; this arm must agree for an in-band Error phase
        // on an otherwise-live stream.
        watch.remove(k);
        _stopIdleTickerIfIdle();
        pendingPrompts.remove(k);
        syncState[k] = SyncState.error;
        syncMessage[k] = message;
      case SyncPhase_Stopped():
        watch.remove(k);
        _stopIdleTickerIfIdle();
        reload();
    }
    notifyListeners();
  }

  void stopWatchItem(ProjectItem item, EnvSummary env) {
    stopWatch(folder: item.summary.folder, env: env.name);
    // The registry cancels; SyncPhase_Stopped clears the entry. Mark it
    // stopping now so the button flips immediately.
    watch[envKey(item.summary.folder, env.name)]?.running = false;
    notifyListeners();
  }

  void answer(PendingPrompt p, String key) {
    answerPrompt(folder: p.folder, env: p.env, promptId: p.id, answer: key);
    pendingPrompts.remove(envKey(p.folder, p.env));
    notifyListeners();
  }

  void clearError() {
    lastError = null;
    notifyListeners();
  }
}
