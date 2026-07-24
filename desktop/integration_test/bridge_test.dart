// Runtime verification of the Rust bridge, executed against the real native
// library on the host OS (run with: `flutter test integration_test -d macos`).
import 'dart:io';

import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/rust/frb_generated.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  setUpAll(() async {
    await RustLib.init();
  });

  test('rdcVersion returns the embedded rdc version', () async {
    final v = await rdcVersion();
    expect(v, isNotNull);
    expect(v!.isNotEmpty, true);
  });

  test('add / list / validate round-trip on a temp folder', () async {
    final dir = Directory.systemTemp.createTempSync('rossum_local_it');
    try {
      final added = await addConnection(
        parent: dir.path,
        input: AddConnectionInput(
          name: 'Acme Prod',
          apiBase: 'https://example.test/api/v1/',
          orgId: BigInt.from(42),
          authKind: AuthKind.token,
          token: 'tok-123',
        ),
      );
      // API base trailing slash is trimmed by the bridge.
      expect(added.apiBase, 'https://example.test/api/v1');
      expect(added.orgId, BigInt.from(42));
      expect(File('${dir.path}/${added.id}/rdc.toml').existsSync(), true);
      expect(
        File('${dir.path}/${added.id}/secrets/main.secrets.json').existsSync(),
        true,
      );

      final list = await listConnections(parent: dir.path);
      expect(list.length, 1);
      expect(list.first.id, added.id);

      final validated =
          await validateExistingProject(path: '${dir.path}/${added.id}');
      expect(validated.orgId, BigInt.from(42));
    } finally {
      dir.deleteSync(recursive: true);
    }
  });

  test('addConnection rejects an empty token', () async {
    final dir = Directory.systemTemp.createTempSync('rossum_local_it2');
    try {
      await expectLater(
        addConnection(
          parent: dir.path,
          input: AddConnectionInput(
            name: 'x',
            apiBase: 'https://example.test/api/v1',
            orgId: BigInt.from(1),
            authKind: AuthKind.token,
            token: '',
          ),
        ),
        throwsA(anything),
      );
    } finally {
      dir.deleteSync(recursive: true);
    }
  });

  test('validateExistingProject rejects a non-project folder', () async {
    final dir = Directory.systemTemp.createTempSync('rossum_local_it3');
    try {
      await expectLater(
        validateExistingProject(path: dir.path),
        throwsA(anything),
      );
    } finally {
      dir.deleteSync(recursive: true);
    }
  });
}
