import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/home_page.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

EnvSummary _e(String name, int org) => EnvSummary(
      name: name, apiBase: 'https://x.test/api/v1', orgId: BigInt.from(org),
      authKind: AuthKind.token, lastSyncUnix: 1000, fileCount: BigInt.from(5),
    );

AppState _seeded() {
  final s = AppState(Settings(parentFolder: '/tmp'));
  s.projects = [
    ProjectItem(ProjectSummary(id: 'acme', name: 'acme', folder: '/tmp/acme',
        envs: [_e('dev', 1), _e('prod', 2)]), false),
  ];
  s.selectProject('/tmp/acme');
  return s;
}

void main() {
  testWidgets('sidebar shows env children and selecting one pins it', (t) async {
    final s = _seeded();
    await t.pumpWidget(MaterialApp(
      theme: mdhTheme(Brightness.light),
      home: Scaffold(body: MdhScaffold(
        state: s,
        view: NavView.connection,
        onSelectEnv: (folder, env) => s.selectEnv(folder, env),
      )),
    ));
    await t.pumpAndSettle();
    expect(find.text('dev'), findsOneWidget);
    expect(find.text('prod'), findsOneWidget);
    await t.tap(find.text('prod'));
    await t.pumpAndSettle();
    expect(s.selectedEnv, 'prod');
  });
}
