import 'package:flutter/foundation.dart';

import 'error_text.dart';
import 'rust/api/rdc.dart';
import 'settings.dart';

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
    selectedFolder = folder;
    ProjectItem? p;
    for (final it in projects) {
      if (it.summary.folder == folder) p = it;
    }
    selectedEnv = (p != null && p.summary.envs.isNotEmpty) ? p.summary.envs.first.name : null;
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
    // Re-pin the selected env if it vanished (e.g. removed on disk).
    if (selectedFolder != null && selected != null &&
        !selected!.summary.envs.any((e) => e.name == selectedEnv)) {
      selectedEnv = selected!.summary.envs.isNotEmpty ? selected!.summary.envs.first.name : null;
    }
    if (selectedFolder == null && projects.isNotEmpty) {
      selectProject(projects.first.summary.folder);
    }
    loading = false;
    notifyListeners();
  }

  Future<void> addProjectEntry(AddConnectionInput input) async {
    final parent = _settings.parentFolder;
    if (parent == null) throw Exception('Choose a parent folder first.');
    final s = await addProject(parent: parent, input: input);
    await reload();
    selectProject(s.folder);
  }

  Future<void> editEnvEntry(ProjectItem item, EnvSummary env, EditConnectionInput input) async {
    final updated = await editProject(folder: item.summary.folder, env: env.name, input: input);
    if (item.isExternal && updated.folder != item.summary.folder) {
      final i = _settings.externalPaths.indexOf(item.summary.folder);
      if (i >= 0) {
        _settings.externalPaths[i] = updated.folder;
        _settings.save();
      }
    }
    await reload();
    selectEnv(updated.folder, env.name);
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

  void clearError() {
    lastError = null;
    notifyListeners();
  }
}
