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
      final added = await addProject(
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
      expect(added.envs.first.apiBase, 'https://example.test/api/v1');
      expect(added.envs.first.orgId, BigInt.from(42));
      expect(File('${dir.path}/${added.id}/rdc.toml').existsSync(), true);
      expect(
        File('${dir.path}/${added.id}/secrets/main.secrets.json').existsSync(),
        true,
      );

      final list = await listProjects(parent: dir.path);
      expect(list.length, 1);
      expect(list.first.id, added.id);

      final validated =
          await validateExistingProject(path: '${dir.path}/${added.id}');
      expect(validated.envs.first.orgId, BigInt.from(42));
    } finally {
      dir.deleteSync(recursive: true);
    }
  });

  test('addProject rejects an empty token', () async {
    final dir = Directory.systemTemp.createTempSync('rossum_local_it2');
    try {
      await expectLater(
        addProject(
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

  test('trashProject removes the folder (no Apple Events needed)', () async {
    final parent = Directory.systemTemp.createTempSync('rdc_it_trash');
    try {
      final added = await addProject(
        parent: parent.path,
        input: AddConnectionInput(
          name: 'Trash Me',
          apiBase: 'https://example.test/api/v1',
          orgId: BigInt.from(3),
          authKind: AuthKind.token,
          token: 'tok',
        ),
      );
      final folder = '${parent.path}/${added.id}';
      expect(Directory(folder).existsSync(), true);
      // Must not throw the AppleScript/-1743 error the Finder backend did.
      await trashProject(folder: folder);
      expect(Directory(folder).existsSync(), false);
    } finally {
      parent.deleteSync(recursive: true);
    }
  });

  test('editProject updates api_base/org_id and renames the folder',
      () async {
    final parent = Directory.systemTemp.createTempSync('rdc_it_edit');
    try {
      final added = await addProject(
        parent: parent.path,
        input: AddConnectionInput(
          name: 'before',
          apiBase: 'https://old.test/api/v1',
          orgId: BigInt.from(1),
          authKind: AuthKind.token,
          token: 'tok',
        ),
      );
      final updated = await editProject(
        folder: '${parent.path}/${added.id}',
        env: 'main',
        input: EditConnectionInput(
          name: 'after',
          apiBase: 'https://new.test/api/v1/',
          orgId: BigInt.from(99),
          authKind: AuthKind.token,
          token: null, // blank → keep existing credentials
        ),
      );
      expect(updated.id, 'after');
      expect(Directory('${parent.path}/${added.id}').existsSync(), false);
      expect(Directory('${parent.path}/after').existsSync(), true);
      expect(updated.envs.first.apiBase, 'https://new.test/api/v1'); // trailing slash trimmed
      expect(updated.envs.first.orgId, BigInt.from(99));
      // credentials were left blank, so token auth is preserved
      expect(updated.envs.first.authKind, AuthKind.token);
      expect(
        File('${parent.path}/after/secrets/main.secrets.json').existsSync(),
        true,
      );
    } finally {
      parent.deleteSync(recursive: true);
    }
  });

  test('addEnv / removeEnv round-trip on a managed project', () async {
    final parent = Directory.systemTemp.createTempSync('rdc_it_envs');
    try {
      final added = await addProject(
        parent: parent.path,
        input: AddConnectionInput(
          name: 'multi-env',
          apiBase: 'https://example.test/api/v1',
          orgId: BigInt.from(10),
          authKind: AuthKind.token,
          token: 'tok-main',
        ),
      );
      final folder = '${parent.path}/${added.id}';

      final withProd = await addEnv(
        folder: folder,
        input: AddEnvInput(
          name: 'prod',
          apiBase: 'https://prod.example.test/api/v1',
          orgId: BigInt.from(20),
          authKind: AuthKind.token,
          token: 'tok-prod',
        ),
      );
      expect(withProd.envs.map((e) => e.name).toList(), ['main', 'prod']);

      var list = await listProjects(parent: parent.path);
      expect(list.length, 1);
      expect(list.first.envs.map((e) => e.name).toList(), ['main', 'prod']);
      expect(
        File('$folder/secrets/prod.secrets.json').existsSync(),
        true,
      );

      final backToMain = await removeEnv(folder: folder, env: 'prod');
      expect(backToMain, isNotNull);
      expect(backToMain!.envs.map((e) => e.name).toList(), ['main']);

      list = await listProjects(parent: parent.path);
      expect(list.length, 1);
      expect(list.first.envs.map((e) => e.name).toList(), ['main']);

      // Removing the last env trashes the whole project.
      final afterLastRemoval = await removeEnv(folder: folder, env: 'main');
      expect(afterLastRemoval, isNull);
      expect(Directory(folder).existsSync(), false);
    } finally {
      if (parent.existsSync()) parent.deleteSync(recursive: true);
    }
  });

  test('listProjects surfaces a CLI project with no main env', () async {
    final parent = Directory.systemTemp.createTempSync('rdc_it_multienv');
    try {
      final dir = Directory('${parent.path}/cli')..createSync();
      File('${dir.path}/rdc.toml').writeAsStringSync(
          '[envs.dev]\napi_base = "https://d.test/api/v1"\norg_id = 1\n'
          '[envs.prod]\napi_base = "https://p.test/api/v1"\norg_id = 2\n');
      final list = await listProjects(parent: parent.path);
      expect(list.length, 1);
      final envs = list.first.envs.map((e) => e.name).toList();
      expect(envs, ['dev', 'prod']);
    } finally {
      parent.deleteSync(recursive: true);
    }
  });
}
