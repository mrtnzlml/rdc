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

  /// Per-env watch options, keyed like `AppState.envKey` — folder and env
  /// joined by a NUL byte, since a folder path may itself contain a space.
  /// Only `pollSecs` today; an absent entry means the 60s default.
  Map<String, dynamic> watch;

  /// Projects whose owner has seen the one-time notice that Sync now writes
  /// to Rossum. Per project, not per env — the surprise is about the app,
  /// not about any one connection.
  List<String> ackTwoWay;

  /// Every key in the on-disk JSON this build does not recognise, kept
  /// verbatim so a save never destroys them. That covers `promoteDefaults`
  /// (written by builds that still had the Promote panel — downgrading must
  /// still find it) and any key a future build adds.
  final Map<String, dynamic> extra;

  /// Keys this build owns. Anything else lands in [extra].
  static const _known = {'parentFolder', 'externalPaths', 'watch', 'ackTwoWay'};

  /// Same NUL-separated key shape as `AppState.envKey`.
  static String _watchKey(String folder, String env) => '$folder\u0000$env';

  /// Overrides the on-disk file used by the instance method [save] below.
  /// `null` (the production default) means "use the real per-user file" (see
  /// [_file]). Tests pass a temp file here so `flutter test` can never write
  /// the developer's real `~/.rdc-desktop/settings.json`. Note this only
  /// affects [save]: [load] is `static` and always reads the real per-user
  /// file via [_file] directly, so it never sees this override — a test
  /// that needs a loaded instance should go through [fromJson] instead.
  final File? _overrideFile;

  Settings({
    this.parentFolder,
    List<String>? externalPaths,
    Map<String, dynamic>? watch,
    List<String>? ackTwoWay,
    Map<String, dynamic>? extra,
    File? file,
  })  : externalPaths = externalPaths ?? [],
        watch = watch ?? {},
        ackTwoWay = ackTwoWay ?? [],
        extra = extra ?? {},
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
  /// falls back to its default. `file` is forwarded to the constructor so a
  /// test can decode a fixture and still route the resulting instance's
  /// [save] at a temp file instead of the real per-user one.
  static Settings fromJson(Map<String, dynamic> m, {File? file}) => Settings(
        parentFolder: m['parentFolder'] as String?,
        externalPaths:
            (m['externalPaths'] as List?)?.map((e) => e as String).toList() ?? [],
        watch: (m['watch'] as Map?)?.cast<String, dynamic>() ?? {},
        ackTwoWay:
            (m['ackTwoWay'] as List?)?.map((e) => e as String).toList() ?? [],
        extra: {
          for (final e in m.entries)
            if (!_known.contains(e.key)) e.key: e.value,
        },
        file: file,
      );

  /// Known keys are written last so a stale value in [extra] can never
  /// shadow the live one.
  Map<String, dynamic> toJson() => {
        ...extra,
        'parentFolder': parentFolder,
        'externalPaths': externalPaths,
        'watch': watch,
        'ackTwoWay': ackTwoWay,
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

  /// The saved poll interval for `env` under `folder`, or `null` when this
  /// project has never had one set (the caller then falls back to 60s).
  int? pollSecsFor(String folder, String env) {
    final v = watch[_watchKey(folder, env)];
    return v is Map ? v['pollSecs'] as int? : null;
  }

  /// Sets (or, with `null`, clears) the poll interval for `env` under
  /// `folder`. Persists immediately.
  void setPollSecs(String folder, String env, int? secs) {
    final k = _watchKey(folder, env);
    if (secs == null) {
      watch.remove(k);
    } else {
      watch[k] = {'pollSecs': secs};
    }
    save();
  }

  bool hasAckedTwoWay(String folder) => ackTwoWay.contains(folder);

  /// Records that `folder`'s owner has seen the one-time two-way notice.
  /// Persists immediately; a no-op (no redundant write) if already recorded.
  void ackTwoWayFor(String folder) {
    if (!ackTwoWay.contains(folder)) {
      ackTwoWay.add(folder);
      save();
    }
  }
}
