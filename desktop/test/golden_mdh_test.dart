// Renders the MDH-style UI to PNGs for visual fidelity checks against the
// approved proposal. Run: flutter test --update-goldens test/golden_mdh_test.dart
import 'dart:io';

import 'package:desktop/src/app.dart';
import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/home_page.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

ProjectItem _proj(String name, List<EnvSummary> envs, {bool external = false, String? folder}) =>
    ProjectItem(ProjectSummary(id: name, name: name, folder: folder ?? '/tmp/Rossum/$name', envs: envs), external);

EnvSummary _env(String name, int org, {int? lastSync, int files = 0}) => EnvSummary(
      name: name, apiBase: 'https://acme.rossum.app/api/v1', orgId: BigInt.from(org),
      authKind: AuthKind.token, lastSyncUnix: lastSync, fileCount: BigInt.from(files));

AppState _seeded() {
  // A long-ish path so the "Folder" spec card wraps to multiple lines — this
  // is what exercises the equal-height grid alignment in the connection view.
  const sel = '/tmp/Rossum/acme-invoices-eu-prod-primary';
  final s = AppState(Settings(parentFolder: '/tmp/Rossum'));
  s.projects = [
    // Multi-env project — exercises the two-level sidebar tree (project row
    // + indented env rows, one selected).
    _proj('acme-invoices', [
      _env('dev', 123456, files: 64),
      _env('prod', 123456, lastSync: 1000, files: 128),
    ], folder: sel),
    _proj('acme-orders', [_env('main', 123457)]),
    _proj('globex-dev', [_env('main', 654321, files: 210)]),
    _proj('widgets-eu', [_env('main', 778899, lastSync: 1000, files: 302)], external: true),
  ];
  s.selectProject(sel);
  s.selectEnv(sel, 'prod');
  s.syncState[s.envKey('/tmp/Rossum/globex-dev', 'main')] = SyncState.error;
  s.syncMessage[s.envKey('/tmp/Rossum/globex-dev', 'main')] = "couldn't sign in (401)";
  s.syncLog[s.envKey(sel, 'prod')] = [
    '\x1B[2m14:12:03\x1B[0m \x1B[38;2;120;180;90mPULL\x1B[0m   schemas … 12 ok',
    '\x1B[2m14:12:05\x1B[0m \x1B[38;2;120;180;90mPULL\x1B[0m   hooks … 8 ok',
    '\x1B[2m14:12:07\x1B[0m \x1B[1;38;2;237;142;71mWRITE\x1B[0m  queues … 3 ok',
    '✓ done · 128 files',
  ];
  return s;
}

/// A real on-disk connection folder for the Files tab. Fixed basename so the
/// breadcrumb width is deterministic across runs. The Files tab browses
/// `envs/<env>/`, so every previewable file lives under `envs/main/`.
Directory _filesFixture() {
  final root = Directory('${Directory.systemTemp.path}/rdc_golden_conn');
  if (root.existsSync()) root.deleteSync(recursive: true);
  Directory('${root.path}/envs/main/hooks').createSync(recursive: true);
  Directory('${root.path}/envs/main/queues').createSync(recursive: true);
  Directory('${root.path}/envs/main/schemas').createSync(recursive: true);
  File('${root.path}/envs/main/hooks/validate_invoice.json').writeAsStringSync('x' * 2000);
  File('${root.path}/envs/main/queues/invoices.json').writeAsStringSync('x' * 4200);
  // multi-line content so the read-only preview shows real, highlightable lines
  File('${root.path}/envs/main/hooks/config.toml').writeAsStringSync(
      '# environment map\n[env.dev]\nqueue = "invoices"\nschema = "invoices"\nhook = "validate_invoice"\n');
  File('${root.path}/envs/main/schemas/invoices.json').writeAsStringSync(
      '{\n  "queue": "invoices",\n  "active": true,\n  "count": 42,\n  "tags": ["a", "b"]\n}\n');
  File('${root.path}/envs/main/.gitignore').writeAsStringSync('x' * 40); // hidden → skipped
  return root;
}

AppState _filesState(Directory root) {
  final s = AppState(Settings(parentFolder: Directory.systemTemp.path));
  s.projects = [_proj('acme-invoices', [_env('main', 123456, lastSync: 1000, files: 128)], folder: root.path)];
  s.selectProject(root.path);
  s.selectEnv(root.path, 'main'); // pin the env so _ConnMain (Files tab) renders, not the Project view
  return s;
}

/// A multi-env project with no env pinned — exercises the Project view
/// (environments table) rendered when `selectedEnv == null`.
AppState _projectViewState() {
  const sel = '/tmp/Rossum/acme-invoices-eu-prod-primary';
  final s = AppState(Settings(parentFolder: '/tmp/Rossum'));
  s.projects = [
    _proj('acme-invoices', [
      _env('dev', 123456, files: 64),
      _env('prod', 123456, lastSync: 1000, files: 128),
    ], folder: sel),
  ];
  s.selectProject(sel);
  return s;
}

Widget _wrap(Brightness b, Widget child) => MaterialApp(
      debugShowCheckedModeBanner: false,
      theme: mdhTheme(b),
      home: Scaffold(body: child),
    );

void main() {
  Future<void> shot(WidgetTester t, Widget w, String file,
      {Size size = const Size(1080, 660)}) async {
    t.view.physicalSize = size;
    t.view.devicePixelRatio = 1.0;
    addTearDown(t.view.resetPhysicalSize);
    addTearDown(t.view.resetDevicePixelRatio);
    await t.pumpWidget(w);
    await t.pumpAndSettle();
    await expectLater(find.byType(MaterialApp), matchesGoldenFile(file));
  }

  testWidgets('empty state', (t) async {
    await shot(t, RdcApp(state: AppState(Settings())), 'goldens/mdh_empty.png', size: const Size(900, 560));
  });

  testWidgets('connection view — light', (t) async {
    await shot(
        t,
        _wrap(Brightness.light,
            MdhScaffold(state: _seeded(), view: NavView.connection, onSelectEnv: (f, e) {})),
        'goldens/mdh_conn_light.png');
  });

  testWidgets('connection view — dark', (t) async {
    await shot(
        t,
        _wrap(Brightness.dark,
            MdhScaffold(state: _seeded(), view: NavView.connection, onSelectEnv: (f, e) {})),
        'goldens/mdh_conn_dark.png');
  });

  testWidgets('fleet overview — light', (t) async {
    await shot(
        t,
        _wrap(Brightness.light,
            MdhScaffold(state: _seeded(), view: NavView.overview, onSelectEnv: (f, e) {})),
        'goldens/mdh_fleet_light.png');
  });

  testWidgets('project view — light', (t) async {
    await shot(
        t,
        _wrap(
            Brightness.light,
            MdhScaffold(
                state: _projectViewState(),
                view: NavView.connection,
                onSelectEnv: (f, e) {},
                onAddEnv: (_) {},
                onEditEnv: (_, _) {},
                onRemoveEnv: (_, _) {},
                onAdd: () {})),
        'goldens/mdh_project_light.png');
  });

  testWidgets('files tab — light', (t) async {
    final root = _filesFixture();
    addTearDown(() => root.deleteSync(recursive: true));
    await shot(
        t,
        _wrap(
            Brightness.light,
            MdhScaffold(
                state: _filesState(root),
                view: NavView.connection,
                activeTab: 'files',
                onSelectEnv: (f, e) {})),
        'goldens/mdh_files_light.png');
  });

  testWidgets('files preview — light', (t) async {
    final root = _filesFixture();
    addTearDown(() => root.deleteSync(recursive: true));
    t.view.physicalSize = const Size(1080, 660);
    t.view.devicePixelRatio = 1.0;
    addTearDown(t.view.resetPhysicalSize);
    addTearDown(t.view.resetDevicePixelRatio);
    await t.pumpWidget(_wrap(
        Brightness.light,
        MdhScaffold(
            state: _filesState(root), view: NavView.connection, activeTab: 'files', onSelectEnv: (f, e) {})));
    await t.pumpAndSettle();
    await t.tap(find.text('hooks')); // descend into envs/main/hooks/
    await t.pumpAndSettle();
    await t.tap(find.text('config.toml')); // open the read-only preview (TOML highlighting)
    await t.pumpAndSettle();
    await expectLater(find.byType(MaterialApp), matchesGoldenFile('goldens/mdh_files_preview_light.png'));
  });

  testWidgets('files preview json — light', (t) async {
    final root = _filesFixture();
    addTearDown(() => root.deleteSync(recursive: true));
    t.view.physicalSize = const Size(1080, 660);
    t.view.devicePixelRatio = 1.0;
    addTearDown(t.view.resetPhysicalSize);
    addTearDown(t.view.resetDevicePixelRatio);
    await t.pumpWidget(_wrap(
        Brightness.light,
        MdhScaffold(
            state: _filesState(root), view: NavView.connection, activeTab: 'files', onSelectEnv: (f, e) {})));
    await t.pumpAndSettle();
    await t.tap(find.text('schemas')); // descend into envs/main/schemas/
    await t.pumpAndSettle();
    await t.tap(find.text('invoices.json')); // JSON syntax highlighting
    await t.pumpAndSettle();
    await expectLater(find.byType(MaterialApp), matchesGoldenFile('goldens/mdh_files_preview_json_light.png'));
  });
}
