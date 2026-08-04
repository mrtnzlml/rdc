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
  test('selectProject shows the Project view (no env pinned)', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    s.projects = [_p('/tmp/acme', ['main', 'prod'])];
    s.selectProject('/tmp/acme');
    expect(s.selectedFolder, '/tmp/acme');
    expect(s.selectedEnv, isNull);           // Project view
    expect(s.selectedEnvSummary, isNull);
    s.selectEnv('/tmp/acme', 'prod');
    expect(s.selectedEnv, 'prod');           // env view
  });
}
