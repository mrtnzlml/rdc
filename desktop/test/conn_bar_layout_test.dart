// _ConnBar switches between a one-line header (title + five un-flexed
// buttons, exactly the pre-Task-14 layout) and a wrapping fallback based
// on a hand-measured constant (`_actionsIntrinsicWidth` in
// home_page.dart) that nothing else enforces — a longer button label
// could silently push the real button row past what that constant
// assumes, and `_ConnBar` would still pick the one-line branch and
// hard-overflow, with no compile-time or runtime signal.
//
// These tests pin the *behaviour* at fixed window widths that bracket
// today's known crossover (empirically located between 840px, wrapped,
// and 845px, one line) rather than reaching into the private constant:
// above it, the header must render on one line with no RenderFlex
// overflow; below it, it must degrade to wrapping without throwing.
// Because the test widths are fixed and the branch condition is not,
// a future label growing enough to push the real button row past the
// (unchanged) 860px case turns that case into a real overflow — the
// suite goes red at the right place instead of shipping the regression.
//
// Falsification performed by hand while writing this test: temporarily
// changing the 'Remove' button's label to 'Remove environment entirely'
// in home_page.dart made the 860px case below fail with "A RenderFlex
// overflowed", exactly as intended; reverting the label made it pass
// again. See the task report for the full transcript.
import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/home_page.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

EnvSummary _e(String name, int org) => EnvSummary(
      name: name, apiBase: 'https://acme.rossum.app/api/v1', orgId: BigInt.from(org),
      authKind: AuthKind.token, lastSyncUnix: 1000, fileCount: BigInt.from(128),
    );

AppState _connState({required bool error}) {
  final s = AppState(Settings(parentFolder: '/tmp/Rossum'));
  s.projects = [
    ProjectItem(ProjectSummary(id: 'acme-invoices', name: 'acme-invoices', folder: '/tmp/Rossum/acme-invoices',
        envs: [_e('dev', 123456), _e('prod', 123456)]), false),
  ];
  s.selectProject('/tmp/Rossum/acme-invoices');
  s.selectEnv('/tmp/Rossum/acme-invoices', 'prod');
  if (error) s.syncState[s.envKey('/tmp/Rossum/acme-invoices', 'prod')] = SyncState.error;
  return s;
}

/// Number of distinct vertical bands among the five action buttons'
/// labels. All are MdhBtns with identical internal padding, so comparing
/// their text tops is an exact same-line/different-line signal (unlike
/// comparing against the title's differently padded, two-line Column).
int _actionLineCount(WidgetTester t, String syncLabel) {
  final ys = <double>{};
  for (final label in [syncLabel, 'Watch', 'Edit', 'Reveal', 'Remove']) {
    final f = find.text(label);
    if (f.evaluate().isNotEmpty) ys.add(t.getTopLeft(f).dy.roundToDouble());
  }
  return ys.length;
}

Future<void> _pumpAt(WidgetTester t, double windowWidth, {required bool error}) async {
  t.view.physicalSize = Size(windowWidth, 660);
  t.view.devicePixelRatio = 1.0;
  addTearDown(t.view.resetPhysicalSize);
  addTearDown(t.view.resetDevicePixelRatio);
  await t.pumpWidget(MaterialApp(
    debugShowCheckedModeBanner: false,
    theme: mdhTheme(Brightness.light),
    home: Scaffold(body: MdhScaffold(state: _connState(error: error), view: NavView.connection)),
  ));
  await t.pumpAndSettle();
}

void main() {
  // Window widths, not _ConnBar's own content width: the sidebar (250px),
  // resizer (5px) and the bar's own horizontal padding (36px) sit between
  // the two, so these are ~291px wider than the ~552px content-width
  // boundary they bracket.
  const above = 860.0; // one line today, with room to spare
  const below = 825.0; // wrapped today

  for (final MapEntry(key: label, value: error) in {'Sync': false, 'Retry': true}.entries) {
    testWidgets('renders on one line above the crossover ($label)', (t) async {
      await _pumpAt(t, above, error: error);
      expect(_actionLineCount(t, label), 1,
          reason: 'expected a single-line header at ${above}px; if this just '
              'started failing, a button label likely grew past what '
              '_actionsIntrinsicWidth in home_page.dart assumes');
    });

    testWidgets('degrades to wrapping (not overflow) below the crossover ($label)', (t) async {
      await _pumpAt(t, below, error: error);
      expect(_actionLineCount(t, label), greaterThan(1),
          reason: 'expected the action buttons to wrap onto more than one '
              'line at ${below}px');
    });
  }
}
