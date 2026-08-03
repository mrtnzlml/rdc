import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter_test/flutter_test.dart';

ProjectItem _p(String folder, List<EnvSummary> envs, {bool ext = false}) => ProjectItem(
      ProjectSummary(id: folder, name: folder.split('/').last, folder: folder, envs: envs),
      ext,
    );

EnvSummary _e(String name, int org) => EnvSummary(
      name: name, apiBase: 'https://x.test/api/v1', orgId: BigInt.from(org),
      authKind: AuthKind.token, lastSyncUnix: null, fileCount: BigInt.zero,
    );

void main() {
  test('selecting a project picks its first env; selecting an env pins it', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    s.projects = [_p('/tmp/acme', [_e('dev', 1), _e('prod', 2)])];
    s.selectProject('/tmp/acme');
    expect(s.selected!.summary.folder, '/tmp/acme');
    expect(s.selectedEnv, 'dev');
    expect(s.selectedEnvSummary!.orgId, BigInt.from(1));
    s.selectEnv('/tmp/acme', 'prod');
    expect(s.selectedEnvSummary!.orgId, BigInt.from(2));
  });

  test('sync state is keyed per (folder, env)', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    s.syncState[s.envKey('/tmp/acme', 'dev')] = SyncState.running;
    expect(s.syncState[s.envKey('/tmp/acme', 'prod')], isNull);
    expect(s.syncState[s.envKey('/tmp/acme', 'dev')], SyncState.running);
  });
}
