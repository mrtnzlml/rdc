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

  /// Per-project promote defaults, keyed by project folder. Each value is
  /// `{src, tgt, mirror, policy}` (`policy` is a [ConflictPolicy] name
  /// string) — the last direction/options the user prepared for that
  /// project, so re-selecting it re-arms the same picks instead of falling
  /// back to the first-two-envs default.
  Map<String, Map<String, dynamic>> promoteDefaults;

  /// Overrides the on-disk file used by the instance methods [save]/[load]
  /// below. `null` (the production default) means "use the real per-user
  /// file" (see [_file]). Tests pass a temp file here so `flutter test` can
  /// never read or write the developer's real `~/.rdc-desktop/settings.json`.
  final File? _overrideFile;

  Settings({
    this.parentFolder,
    List<String>? externalPaths,
    Map<String, Map<String, dynamic>>? promoteDefaults,
    File? file,
  })  : externalPaths = externalPaths ?? [],
        promoteDefaults = promoteDefaults ?? {},
        _overrideFile = file;

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

  /// Pure (no disk I/O) deserialization, so it's unit-testable and `load`
  /// can delegate to it. Tolerates a missing/legacy file: any absent key
  /// falls back to its default (in particular `promoteDefaults` → `{}`).
  static Settings fromJson(Map<String, dynamic> m) => Settings(
        parentFolder: m['parentFolder'] as String?,
        externalPaths:
            (m['externalPaths'] as List?)?.map((e) => e as String).toList() ??
                [],
        promoteDefaults: (m['promoteDefaults'] as Map?)?.map(
              (k, v) => MapEntry(k as String, Map<String, dynamic>.from(v as Map)),
            ) ??
            {},
      );

  /// Pure (no disk I/O) serialization; `save` delegates to it.
  Map<String, dynamic> toJson() => {
        'parentFolder': parentFolder,
        'externalPaths': externalPaths,
        'promoteDefaults': promoteDefaults,
      };

  static Settings load() {
    try {
      final f = _file();
      if (!f.existsSync()) return Settings();
      final m = jsonDecode(f.readAsStringSync()) as Map<String, dynamic>;
      return Settings.fromJson(m);
    } catch (_) {
      return Settings();
    }
  }

  void save() {
    try {
      (_overrideFile ?? _file()).writeAsStringSync(
        const JsonEncoder.withIndent('  ').convert(toJson()),
      );
    } catch (_) {
      // Settings are best-effort; a write failure must not crash the app.
    }
  }
}
