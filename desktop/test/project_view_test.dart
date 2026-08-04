import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/home_page.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

EnvSummary _e(String name, int org) => EnvSummary(
      name: name, apiBase: 'https://x.test/api/v1', orgId: BigInt.from(org),
      authKind: AuthKind.token, lastSyncUnix: null, fileCount: BigInt.zero,
    );

AppState _seeded() {
  final s = AppState(Settings(parentFolder: '/tmp'));
  s.projects = [
    ProjectItem(ProjectSummary(id: 'acme', name: 'acme', folder: '/tmp/acme',
        envs: [_e('main', 1), _e('prod', 2)]), false),
  ];
  s.selectProject('/tmp/acme');
  return s;
}

void main() {
  testWidgets('Project view lists envs and wires select/add/edit/remove/sync-all', (t) async {
    final s = _seeded();
    String? selectedFolder, selectedEnv;
    ProjectItem? addedTo, editedItem, removedItem;
    EnvSummary? editedEnv, removedEnv;
    final syncedEnvs = <String>[];

    await t.pumpWidget(MaterialApp(
      theme: mdhTheme(Brightness.light),
      home: Scaffold(body: MdhScaffold(
        state: s,
        view: NavView.connection,
        onSelectConn: (folder) => s.selectProject(folder),
        onSelectEnv: (folder, env) {
          selectedFolder = folder;
          selectedEnv = env;
          s.selectEnv(folder, env);
        },
        onSync: (p, e) => syncedEnvs.add(e.name),
        onAddEnv: (p) => addedTo = p,
        onEditEnv: (p, e) {
          editedItem = p;
          editedEnv = e;
        },
        onRemoveEnv: (p, e) {
          removedItem = p;
          removedEnv = e;
        },
      )),
    ));
    await t.pumpAndSettle();

    // A project (not an env) is selected -> Project view, not _ConnMain.
    expect(s.selectedEnv, isNull);
    expect(find.text('No environment selected'), findsNothing);

    // One row per env in the Environments table. The cell text combines
    // project + env name (like the Fleet table) so it can't collide with the
    // sidebar's bare env-name rows ('main'/'prod') under a plain find.text.
    expect(find.text('acme · main'), findsOneWidget);
    expect(find.text('acme · prod'), findsOneWidget);
    expect(find.text('Add environment'), findsOneWidget);

    await t.tap(find.text('Add environment'));
    await t.pump();
    expect(addedTo?.summary.folder, '/tmp/acme');

    // Tapping an env row selects it.
    await t.tap(find.text('acme · prod'));
    await t.pump();
    expect(selectedFolder, '/tmp/acme');
    expect(selectedEnv, 'prod');

    // Per-row Edit/Remove actions (compact icon buttons; tooltip is their label).
    expect(find.byTooltip('Edit'), findsNWidgets(2));
    await t.tap(find.byTooltip('Edit').first);
    await t.pump();
    expect(editedItem?.summary.folder, '/tmp/acme');
    expect(editedEnv?.name, 'main');

    expect(find.byTooltip('Remove'), findsNWidgets(2));
    await t.tap(find.byTooltip('Remove').last);
    await t.pump();
    expect(removedItem?.summary.folder, '/tmp/acme');
    expect(removedEnv?.name, 'prod');

    // "Sync all envs" syncs every env of this project.
    await t.tap(find.text('Sync all envs'));
    await t.pump();
    expect(syncedEnvs, containsAll(['main', 'prod']));
  });
}
