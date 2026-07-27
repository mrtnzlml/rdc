import 'dart:convert';
import 'dart:io';

/// App-private settings persisted as JSON under the user's home directory.
///
/// Deliberately uses plain `dart:io` (no `shared_preferences`/`path_provider`
/// plugin) so the first cross-platform build needs no extra native pods. This
/// is app-local state only — the shared on-disk rdc contract (rdc.toml,
/// secrets/, envs/, .rdc/state) is never touched here; the Rust bridge owns it.
class Settings {
  String? parentFolder;
  List<String> externalPaths;

  Settings({this.parentFolder, List<String>? externalPaths})
      : externalPaths = externalPaths ?? [];

  static File _file() {
    final home = Platform.environment['HOME'] ??
        Platform.environment['USERPROFILE'] ??
        Directory.current.path;
    final sep = Platform.pathSeparator;
    final dir = Directory('$home$sep.rdc-desktop');
    if (!dir.existsSync()) dir.createSync(recursive: true);
    final file = File('${dir.path}${sep}settings.json');
    // One-time migration from the pre-rename ~/.rossum_local location.
    if (!file.existsSync()) {
      final legacy = File('$home$sep.rossum_local${sep}settings.json');
      if (legacy.existsSync()) {
        try {
          file.writeAsStringSync(legacy.readAsStringSync());
        } catch (_) {}
      }
    }
    return file;
  }

  static Settings load() {
    try {
      final f = _file();
      if (!f.existsSync()) return Settings();
      final m = jsonDecode(f.readAsStringSync()) as Map<String, dynamic>;
      return Settings(
        parentFolder: m['parentFolder'] as String?,
        externalPaths:
            (m['externalPaths'] as List?)?.map((e) => e as String).toList() ??
                [],
      );
    } catch (_) {
      return Settings();
    }
  }

  void save() {
    try {
      _file().writeAsStringSync(
        const JsonEncoder.withIndent('  ').convert({
          'parentFolder': parentFolder,
          'externalPaths': externalPaths,
        }),
      );
    } catch (_) {
      // Settings are best-effort; a write failure must not crash the app.
    }
  }
}
