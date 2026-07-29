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

ConnItem _conn(String name, int org,
    {bool external = false, int? lastSync, int files = 0, String? folder}) {
  return ConnItem(
    ConnectionSummary(
      id: name,
      name: name,
      apiBase: 'https://acme.rossum.app/api/v1',
      orgId: BigInt.from(org),
      folder: folder ?? '/tmp/Rossum/$name',
      authKind: AuthKind.token,
      lastSyncUnix: lastSync,
      fileCount: BigInt.from(files),
    ),
    external,
  );
}

AppState _seeded() {
  final s = AppState(Settings(parentFolder: '/tmp/Rossum'));
  s.connections = [
    _conn('acme-invoices', 123456, lastSync: 1000, files: 128),
    _conn('acme-orders', 123457),
    _conn('globex-dev', 654321, files: 210),
    _conn('widgets-eu', 778899, external: true, lastSync: 1000, files: 302),
  ];
  s.selectedFolder = '/tmp/Rossum/acme-invoices';
  s.syncState['/tmp/Rossum/globex-dev'] = SyncState.error;
  s.syncMessage['/tmp/Rossum/globex-dev'] = "couldn't sign in (401)";
  s.syncLog['/tmp/Rossum/acme-invoices'] = [
    '\x1B[2m14:12:03\x1B[0m \x1B[38;2;120;180;90mPULL\x1B[0m   schemas … 12 ok',
    '\x1B[2m14:12:05\x1B[0m \x1B[38;2;120;180;90mPULL\x1B[0m   hooks … 8 ok',
    '\x1B[2m14:12:07\x1B[0m \x1B[1;38;2;237;142;71mWRITE\x1B[0m  queues … 3 ok',
    '✓ done · 128 files',
  ];
  return s;
}

/// A real on-disk connection folder for the Files tab. Fixed basename so the
/// breadcrumb width is deterministic across runs.
Directory _filesFixture() {
  final root = Directory('${Directory.systemTemp.path}/rdc_golden_conn');
  if (root.existsSync()) root.deleteSync(recursive: true);
  Directory('${root.path}/envs/main/hooks').createSync(recursive: true);
  Directory('${root.path}/envs/main/queues').createSync(recursive: true);
  File('${root.path}/envs/main/hooks/validate_invoice.json').writeAsStringSync('x' * 2000);
  File('${root.path}/envs/main/queues/invoices.json').writeAsStringSync('x' * 4200);
  File('${root.path}/mapping.toml').writeAsStringSync('x' * 320);
  File('${root.path}/overlay.toml').writeAsStringSync('x' * 90);
  File('${root.path}/.gitignore').writeAsStringSync('x' * 40); // hidden → skipped
  return root;
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
    await shot(t, _wrap(Brightness.light, MdhScaffold(state: _seeded(), view: NavView.connection)), 'goldens/mdh_conn_light.png');
  });

  testWidgets('connection view — dark', (t) async {
    await shot(t, _wrap(Brightness.dark, MdhScaffold(state: _seeded(), view: NavView.connection)), 'goldens/mdh_conn_dark.png');
  });

  testWidgets('fleet overview — light', (t) async {
    await shot(t, _wrap(Brightness.light, MdhScaffold(state: _seeded(), view: NavView.overview)), 'goldens/mdh_fleet_light.png');
  });

  testWidgets('files tab — light', (t) async {
    final root = _filesFixture();
    addTearDown(() => root.deleteSync(recursive: true));
    final s = AppState(Settings(parentFolder: Directory.systemTemp.path));
    s.connections = [_conn('acme-invoices', 123456, lastSync: 1000, files: 128, folder: root.path)];
    s.selectedFolder = root.path;
    await shot(t, _wrap(Brightness.light, MdhScaffold(state: s, view: NavView.connection, activeTab: 'files')), 'goldens/mdh_files_light.png');
  });
}
