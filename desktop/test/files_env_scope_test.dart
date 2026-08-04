// The Files tab must be rooted at the selected environment's snapshot
// (`<folder>/envs/<env>/`), not the project root, and must show a friendly
// empty state when that env has never been synced.
import 'dart:io';

import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/home_page.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

ProjectItem _proj(String name, List<EnvSummary> envs, {String? folder}) =>
    ProjectItem(ProjectSummary(id: name, name: name, folder: folder ?? '/tmp/Rossum/$name', envs: envs), false);

EnvSummary _env(String name, int org, {int? lastSync, int files = 0}) => EnvSummary(
      name: name, apiBase: 'https://acme.rossum.app/api/v1', orgId: BigInt.from(org),
      authKind: AuthKind.token, lastSyncUnix: lastSync, fileCount: BigInt.from(files));

AppState _stateFor(Directory root) {
  final s = AppState(Settings(parentFolder: Directory.systemTemp.path));
  s.projects = [_proj('acme-invoices', [_env('main', 123456, lastSync: 1000, files: 1)], folder: root.path)];
  s.selectProject(root.path);
  s.selectEnv(root.path, 'main'); // pin the env so _ConnMain (Files tab) renders
  return s;
}

Widget _wrap(AppState state) => MaterialApp(
      debugShowCheckedModeBanner: false,
      theme: mdhTheme(Brightness.light),
      home: Scaffold(
        body: MdhScaffold(state: state, view: NavView.connection, activeTab: 'files', onSelectEnv: (_, _) {}),
      ),
    );

void main() {
  testWidgets('Files tab is scoped to envs/<env>, not the project root', (t) async {
    final root = Directory('${Directory.systemTemp.path}/rdc_files_env_scope_test');
    if (root.existsSync()) root.deleteSync(recursive: true);
    addTearDown(() => root.deleteSync(recursive: true));
    Directory('${root.path}/envs/main').createSync(recursive: true);
    File('${root.path}/envs/main/only_in_env.txt').writeAsStringSync('hi');
    File('${root.path}/only_in_root.txt').writeAsStringSync('hi'); // sibling of envs/, must NOT show

    await t.pumpWidget(_wrap(_stateFor(root)));
    await t.pumpAndSettle();

    expect(find.text('only_in_env.txt'), findsOneWidget);
    expect(find.text('only_in_root.txt'), findsNothing);
  });

  testWidgets("Files tab shows a friendly empty state when the env hasn't been synced yet", (t) async {
    final root = Directory('${Directory.systemTemp.path}/rdc_files_env_scope_not_synced_test');
    if (root.existsSync()) root.deleteSync(recursive: true);
    addTearDown(() => root.deleteSync(recursive: true));
    root.createSync(recursive: true); // project folder exists, but no envs/main/ yet

    await t.pumpWidget(_wrap(_stateFor(root)));
    await t.pumpAndSettle();

    expect(find.textContaining("hasn't been synced yet"), findsOneWidget);
    expect(find.textContaining("Couldn't read this folder"), findsNothing);
  });
}
