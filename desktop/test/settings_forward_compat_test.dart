import 'package:desktop/src/settings.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
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
