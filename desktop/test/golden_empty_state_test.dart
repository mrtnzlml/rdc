// Headless render of the first-launch (empty) screen, used to produce a visual
// snapshot without a display. Run: flutter test --update-goldens
import 'package:desktop/src/app.dart';
import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('empty state renders', (tester) async {
    tester.view.physicalSize = const Size(1100, 720);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    // Default Settings() → no parent folder → empty state; no Rust/network.
    await tester.pumpWidget(RossumLocalApp(state: AppState(Settings())));
    await tester.pumpAndSettle();

    expect(find.text('Choose folder…'), findsOneWidget);
    await expectLater(
      find.byType(RossumLocalApp),
      matchesGoldenFile('goldens/empty_state.png'),
    );
  });
}
