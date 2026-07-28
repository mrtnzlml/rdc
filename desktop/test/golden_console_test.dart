// Renders the Console UI to PNGs for visual fidelity checks against the
// approved design. Run: flutter test --update-goldens test/golden_console_test.dart
import 'package:desktop/src/app.dart';
import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/console_theme.dart';
import 'package:desktop/src/home_page.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

ConnItem _conn(String name, int org,
    {bool external = false, int? lastSync, int files = 0}) {
  return ConnItem(
    ConnectionSummary(
      id: name,
      name: name,
      apiBase: 'https://acme.rossum.app/api/v1',
      orgId: BigInt.from(org),
      folder: '/tmp/$name',
      authKind: AuthKind.token,
      lastSyncUnix: lastSync,
      fileCount: BigInt.from(files),
    ),
    external,
  );
}

AppState _seeded() {
  final s = AppState(Settings());
  s.connections = [
    _conn('acme-invoices', 123456, lastSync: 1000, files: 128),
    _conn('acme-orders', 123457),
    _conn('globex-dev', 654321, files: 210),
    _conn('widgets-eu', 778899, external: true, lastSync: 1000, files: 302),
  ];
  s.selectedFolder = '/tmp/acme-invoices';
  s.syncState['/tmp/globex-dev'] = SyncState.error;
  s.syncMessage['/tmp/globex-dev'] = "couldn't sign in (401)";
  // ANSI-colored lines, exactly as rdc emits them (ColorMode::Color).
  s.syncLog['/tmp/acme-invoices'] = [
    '\x1B[2m14:12:03\x1B[0m \x1B[38;2;120;180;90mPULL\x1B[0m   schemas … 12 ok',
    '\x1B[2m14:12:05\x1B[0m \x1B[38;2;120;180;90mPULL\x1B[0m   hooks … 8 ok',
    '\x1B[2m14:12:07\x1B[0m \x1B[1;38;2;237;142;71mWRITE\x1B[0m  queues … 3 ok',
    '✓ done · 128 files',
  ];
  return s;
}

Widget _wrap(Brightness b, Widget child) => MaterialApp(
      debugShowCheckedModeBanner: false,
      theme: consoleTheme(b),
      home: Scaffold(body: child),
    );

void main() {
  Future<void> shot(WidgetTester t, Widget w, String file,
      {Size size = const Size(1000, 640)}) async {
    t.view.physicalSize = size;
    t.view.devicePixelRatio = 1.0;
    addTearDown(t.view.resetPhysicalSize);
    addTearDown(t.view.resetDevicePixelRatio);
    await t.pumpWidget(w);
    await t.pumpAndSettle();
    await expectLater(find.byType(MaterialApp), matchesGoldenFile(file));
  }

  testWidgets('empty state', (t) async {
    await shot(t, RdcApp(state: AppState(Settings())), 'goldens/empty_light.png',
        size: const Size(900, 600));
    expect(find.textContaining('choose folder'), findsOneWidget);
  });

  testWidgets('main — light', (t) async {
    await shot(t, _wrap(Brightness.light, ConsoleMain(state: _seeded())), 'goldens/main_light.png');
  });

  testWidgets('main — dark', (t) async {
    await shot(t, _wrap(Brightness.dark, ConsoleMain(state: _seeded())), 'goldens/main_dark.png');
  });
}
