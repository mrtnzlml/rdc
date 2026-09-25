import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('canRenameEnv refuses rename while the env is mid-sync, allows it otherwise', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    const folder = '/tmp/acme';
    const env = 'prod';

    // No sync state recorded yet -> renaming is fine.
    expect(s.canRenameEnv(folder, env), isTrue);

    // Mid-sync -> refuse.
    s.syncState[s.envKey(folder, env)] = SyncState.running;
    expect(s.canRenameEnv(folder, env), isFalse);

    // Sync finished (done) -> fine again.
    s.syncState[s.envKey(folder, env)] = SyncState.done;
    expect(s.canRenameEnv(folder, env), isTrue);

    // Sync errored out -> also fine (only `running` blocks a rename).
    s.syncState[s.envKey(folder, env)] = SyncState.error;
    expect(s.canRenameEnv(folder, env), isTrue);

    // A different env's sync state must not affect this one.
    s.syncState[s.envKey(folder, 'dev')] = SyncState.running;
    expect(s.canRenameEnv(folder, env), isTrue);
  });

}
