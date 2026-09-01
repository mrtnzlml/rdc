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
      'watch': {
        '/tmp/acme dev': {'pollSecs': 120},
      },
      'ackTwoWay': ['/tmp/acme'],
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
    expect(written['watch'], {
      '/tmp/acme dev': {'pollSecs': 120},
    });
    expect(written['ackTwoWay'], ['/tmp/acme']);
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

  test('pollSecs round-trips per env and defaults to null', () {
    // setPollSecs calls save(), so this is sandboxed to a temp file (same
    // pattern as the first test above) rather than reading/writing the
    // developer's real ~/.rdc-desktop/settings.json.
    final tmpDir = Directory.systemTemp.createTempSync('rdc-desktop-settings-test-');
    addTearDown(() {
      if (tmpDir.existsSync()) tmpDir.deleteSync(recursive: true);
    });
    final tmpFile = File('${tmpDir.path}${Platform.pathSeparator}settings.json');
    final s = Settings.fromJson({}, file: tmpFile);

    expect(s.pollSecsFor('/tmp/acme', 'dev'), isNull);
    s.setPollSecs('/tmp/acme', 'dev', 300);
    expect(s.pollSecsFor('/tmp/acme', 'dev'), 300);
    expect(s.pollSecsFor('/tmp/acme', 'prod'), isNull);
  });

  test('ackTwoWay defaults to unacknowledged and hasAckedTwoWay reflects it', () {
    final s = Settings.fromJson({});
    expect(s.hasAckedTwoWay('/tmp/acme'), isFalse);
    s.ackTwoWay.add('/tmp/acme');
    expect(s.hasAckedTwoWay('/tmp/acme'), isTrue);
    expect(s.hasAckedTwoWay('/tmp/beta'), isFalse);
  });

  test('setPollSecs and ackTwoWayFor persist to disk (sandboxed to a temp file)', () {
    // setPollSecs/ackTwoWayFor both call save(), so this must never be able
    // to reach the developer's real ~/.rdc-desktop/settings.json — same
    // sandboxing as the first test above.
    final tmpDir = Directory.systemTemp.createTempSync('rdc-desktop-settings-test-');
    addTearDown(() {
      if (tmpDir.existsSync()) tmpDir.deleteSync(recursive: true);
    });
    final tmpFile = File('${tmpDir.path}${Platform.pathSeparator}settings.json');
    final s = Settings.fromJson({}, file: tmpFile);

    s.setPollSecs('/tmp/acme', 'dev', 45);
    expect(s.pollSecsFor('/tmp/acme', 'dev'), 45);

    expect(s.hasAckedTwoWay('/tmp/acme'), isFalse);
    s.ackTwoWayFor('/tmp/acme');
    expect(s.hasAckedTwoWay('/tmp/acme'), isTrue);
    // Calling it again must not duplicate the entry.
    s.ackTwoWayFor('/tmp/acme');
    expect(s.ackTwoWay.where((f) => f == '/tmp/acme').length, 1);

    // Assert on shape, not on the internal key encoding: the exact
    // `folder`/`env` join character is Settings' own business, not this
    // test's.
    final written = jsonDecode(tmpFile.readAsStringSync()) as Map<String, dynamic>;
    final writtenWatch = written['watch'] as Map;
    expect(writtenWatch.length, 1);
    expect(writtenWatch.values.single, {'pollSecs': 45});
    expect(written['ackTwoWay'], ['/tmp/acme']);

    s.setPollSecs('/tmp/acme', 'dev', null);
    expect(s.pollSecsFor('/tmp/acme', 'dev'), isNull);
    final writtenAfterClear = jsonDecode(tmpFile.readAsStringSync()) as Map<String, dynamic>;
    expect(writtenAfterClear['watch'], <String, dynamic>{});
  });
}
