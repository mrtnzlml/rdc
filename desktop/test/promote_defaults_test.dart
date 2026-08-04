import 'dart:convert';
import 'dart:io';

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
    // Sandboxed: point Settings at a throwaway temp file so this test can
    // never read or write the developer's real ~/.rdc-desktop/settings.json
    // (savePromoteDefaults below calls the real Settings.save()).
    final tmpDir = Directory.systemTemp.createTempSync('rdc-desktop-settings-test-');
    addTearDown(() {
      if (tmpDir.existsSync()) tmpDir.deleteSync(recursive: true);
    });
    final tmpFile = File('${tmpDir.path}${Platform.pathSeparator}settings.json');
    final realHome = Platform.environment['HOME'] ?? Platform.environment['USERPROFILE'];
    final realFile = realHome == null
        ? null
        : File('$realHome${Platform.pathSeparator}.rdc-desktop${Platform.pathSeparator}settings.json');
    final realFileContentsBefore =
        (realFile != null && realFile.existsSync()) ? realFile.readAsStringSync() : null;

    final s = Settings(file: tmpFile);
    final app = AppState(s);
    app.projects = [_p('/tmp/acme', ['dev', 'prod']), _p('/tmp/beta', ['dev', 'prod'])];
    app.selectProject('/tmp/acme');
    app.setPromoteDir('prod', 'dev'); // backport direction
    app.promoteMirror = true;
    app.promotePolicy = ConflictPolicy.useIncoming;
    app.savePromoteDefaults('/tmp/acme');

    // The temp file is where the write actually landed...
    expect(tmpFile.existsSync(), isTrue);
    final written = jsonDecode(tmpFile.readAsStringSync()) as Map<String, dynamic>;
    final acme = written['promoteDefaults']['/tmp/acme'] as Map<String, dynamic>;
    expect(acme['src'], 'prod');
    expect(acme['tgt'], 'dev');
    expect(acme['mirror'], true);
    expect(acme['policy'], 'useIncoming');

    // ...and the real per-user file was left completely untouched.
    if (realFile != null) {
      expect(realFile.existsSync() ? realFile.readAsStringSync() : null, realFileContentsBefore);
    }

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

  test('restorePromoteDefaults does not restore an env that no longer exists on the project', () {
    final tmpDir = Directory.systemTemp.createTempSync('rdc-desktop-settings-test-');
    addTearDown(() {
      if (tmpDir.existsSync()) tmpDir.deleteSync(recursive: true);
    });
    final tmpFile = File('${tmpDir.path}${Platform.pathSeparator}settings.json');

    final s = Settings(file: tmpFile);
    // Seed a saved default naming an env ('staging') that this project no
    // longer has (renamed/removed) — as if written by an older run.
    s.promoteDefaults['/tmp/acme'] = {
      'src': 'staging',
      'tgt': 'dev',
      'mirror': true,
      'policy': 'useIncoming',
    };
    final app = AppState(s);
    app.projects = [_p('/tmp/acme', ['dev', 'prod'])]; // no 'staging' anymore

    app.selectProject('/tmp/acme');

    // 'staging' is gone → src must not be restored (would otherwise render a
    // blank "From" dropdown with an enabled Prepare button). 'dev' still
    // exists so tgt restores normally.
    expect(app.promoteSrc, isNull);
    expect(app.promoteTgt, 'dev');
    // Non-env fields still restore normally.
    expect(app.promoteMirror, true);
    expect(app.promotePolicy, ConflictPolicy.useIncoming);
  });

  test('restorePromoteDefaults tolerates a corrupted saved entry without throwing', () {
    final s = Settings();
    // Hand-edited/corrupted value: wrong types where a String is expected.
    s.promoteDefaults['/tmp/acme'] = {'src': 123, 'tgt': true, 'mirror': 'nope', 'policy': 42};
    final app = AppState(s);
    app.projects = [_p('/tmp/acme', ['dev', 'prod'])];

    expect(() => app.selectProject('/tmp/acme'), returnsNormally);
    expect(app.promoteSrc, isNull);
    expect(app.promoteTgt, isNull);
  });
}
