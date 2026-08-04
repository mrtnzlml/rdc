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
    final s = Settings();
    final app = AppState(s);
    app.projects = [_p('/tmp/acme', ['dev', 'prod']), _p('/tmp/beta', ['dev', 'prod'])];
    app.selectProject('/tmp/acme');
    app.setPromoteDir('prod', 'dev'); // backport direction
    app.promoteMirror = true;
    app.promotePolicy = ConflictPolicy.useIncoming;
    app.savePromoteDefaults('/tmp/acme');
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
}
