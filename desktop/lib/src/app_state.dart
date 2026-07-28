import 'package:flutter/foundation.dart';

import 'error_text.dart';
import 'rust/api/rdc.dart';
import 'settings.dart';

enum SyncState { idle, running, done, error }

/// A discovered connection plus whether it lives outside the parent folder
/// (an "external" project attached via Open Existing). External connections
/// are detached (forgotten) on remove; managed ones are moved to the trash.
class ConnItem {
  final ConnectionSummary summary;
  final bool isExternal;
  const ConnItem(this.summary, this.isExternal);
}

/// Single source of truth for the UI. Wraps the Rust bridge and derives the
/// connection list from disk (parent scan ∪ attached externals), mirroring the
/// retired SwiftUI `ConnectionStore`.
class AppState extends ChangeNotifier {
  AppState(this._settings);

  final Settings _settings;

  List<ConnItem> connections = [];
  String? selectedFolder;
  bool loading = false;
  String? lastError;

  final Map<String, SyncState> syncState = {};
  final Map<String, String> syncMessage = {};

  /// rdc's real, rendered sync-log lines from the current/last run, per folder.
  final Map<String, List<String>> syncLog = {};

  String? get parentFolder => _settings.parentFolder;

  ConnItem? get selected {
    final folder = selectedFolder;
    if (folder == null) return null;
    for (final c in connections) {
      if (c.summary.folder == folder) return c;
    }
    return null;
  }

  void select(String? folder) {
    selectedFolder = folder;
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

    final byFolder = <String, ConnItem>{};
    try {
      final parent = _settings.parentFolder;
      if (parent != null) {
        for (final s in await listConnections(parent: parent)) {
          byFolder[s.folder] = ConnItem(s, false);
        }
      }
      final stillValid = <String>[];
      for (final path in List<String>.from(_settings.externalPaths)) {
        try {
          final s = await validateExistingProject(path: path);
          byFolder.putIfAbsent(s.folder, () => ConnItem(s, true));
          stillValid.add(path);
        } catch (_) {
          // A moved/deleted external project is silently dropped.
        }
      }
      if (stillValid.length != _settings.externalPaths.length) {
        _settings.externalPaths = stillValid;
        _settings.save();
      }
    } catch (e) {
      lastError = errorText(e);
    }

    connections = byFolder.values.toList()
      ..sort((a, b) => a.summary.name
          .toLowerCase()
          .compareTo(b.summary.name.toLowerCase()));
    if (selectedFolder != null &&
        !connections.any((c) => c.summary.folder == selectedFolder)) {
      selectedFolder = null;
    }
    loading = false;
    notifyListeners();
  }

  Future<void> addConnectionEntry(AddConnectionInput input) async {
    final parent = _settings.parentFolder;
    if (parent == null) throw Exception('Choose a parent folder first.');
    final s = await addConnection(parent: parent, input: input);
    await reload();
    selectedFolder = s.folder;
    notifyListeners();
  }

  Future<void> editConnectionEntry(
      ConnItem item, EditConnectionInput input) async {
    final updated =
        await editConnection(folder: item.summary.folder, input: input);
    // If an external connection's folder was renamed, follow it in settings.
    if (item.isExternal && updated.folder != item.summary.folder) {
      final i = _settings.externalPaths.indexOf(item.summary.folder);
      if (i >= 0) {
        _settings.externalPaths[i] = updated.folder;
        _settings.save();
      }
    }
    await reload();
    selectedFolder = updated.folder;
    notifyListeners();
  }

  Future<void> openExisting(String path) async {
    final s = await validateExistingProject(path: path); // throws if invalid
    final parent = _settings.parentFolder;
    final underParent = parent != null && s.folder.startsWith(parent);
    if (!underParent && !_settings.externalPaths.contains(s.folder)) {
      _settings.externalPaths.add(s.folder);
      _settings.save();
    }
    await reload();
    selectedFolder = s.folder;
    notifyListeners();
  }

  /// Managed → move folder to trash; external → forget it (folder untouched).
  Future<void> removeOrDetach(ConnItem item) async {
    if (item.isExternal) {
      _settings.externalPaths.remove(item.summary.folder);
      _settings.save();
    } else {
      await trashConnection(folder: item.summary.folder);
    }
    if (selectedFolder == item.summary.folder) selectedFolder = null;
    await reload();
  }

  Future<void> reveal(String folder) => revealInFileManager(path: folder);

  void sync(ConnItem item) {
    final folder = item.summary.folder;
    syncState[folder] = SyncState.running;
    syncMessage.remove(folder);
    syncLog[folder] = <String>[];
    notifyListeners();

    syncConnection(
      folder: folder,
      apiBase: item.summary.apiBase,
      orgId: item.summary.orgId,
    ).listen(
      (phase) {
        switch (phase) {
          case SyncPhase_Started():
            syncState[folder] = SyncState.running;
          case SyncPhase_Log(:final line):
            (syncLog[folder] ??= <String>[]).add(line);
          case SyncPhase_Done(:final fileCount):
            syncState[folder] = SyncState.done;
            syncMessage[folder] = 'Pulled $fileCount files';
            reload();
          case SyncPhase_Error(:final message):
            syncState[folder] = SyncState.error;
            syncMessage[folder] = message;
        }
        notifyListeners();
      },
      onError: (Object e) {
        syncState[folder] = SyncState.error;
        syncMessage[folder] = errorText(e);
        notifyListeners();
      },
    );
  }

  void clearError() {
    lastError = null;
    notifyListeners();
  }
}
