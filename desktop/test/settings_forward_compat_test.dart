import 'dart:convert';
import 'dart:io';

import 'package:desktop/src/settings.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('unknown keys survive a save to disk and back, without touching the real settings file', () {
    // Sandboxed: point Settings at a throwaway temp file so this test can
    // never read or write the developer's real ~/.rdc-desktop/settings.json.
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

    // A settings file written by an older build that still had promote, plus
    // a hypothetical future key. Neither is known to this build. `Settings.load()`
    // is static and always reads the real per-user path, so it can't be used
    // here — go through fromJson and pass `file:` so save() targets the temp file.
    final s = Settings.fromJson({
      'parentFolder': '/tmp/acme',
      'externalPaths': ['/tmp/beta'],
      'promoteDefaults': {
        '/tmp/acme': {'src': 'dev', 'tgt': 'prod', 'mirror': true, 'policy': 'keepTarget'},
      },
      'somethingFromTheFuture': 42,
    }, file: tmpFile);

    // Mutate a known field and save for real, then read the temp file back
    // from disk — proving the round trip survives a real write, not just an
    // in-memory encode.
    s.parentFolder = '/tmp/acme-renamed';
    s.save();

    expect(tmpFile.existsSync(), isTrue);
    final written = jsonDecode(tmpFile.readAsStringSync()) as Map<String, dynamic>;
    expect(written['parentFolder'], '/tmp/acme-renamed');
    expect(written['promoteDefaults'], isNotNull,
        reason: 'an older build must still find its promote defaults after this build saves');
    expect((written['promoteDefaults'] as Map)['/tmp/acme']['src'], 'dev');
    expect(written['somethingFromTheFuture'], 42);

    // ...and the real per-user file was left completely untouched.
    if (realFile != null) {
      expect(realFile.existsSync() ? realFile.readAsStringSync() : null, realFileContentsBefore);
    }
  });

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

  test('a stale key in extra never shadows the live field', () {
    // Only the public constructor can produce this state: fromJson's _known
    // filter keeps known keys out of extra. Reversing toJson's spread order
    // would flip this assertion — which is the point.
    final s = Settings(parentFolder: '/tmp/live', extra: {'parentFolder': '/tmp/stale'});
    expect(s.toJson()['parentFolder'], '/tmp/live');
  });
}
