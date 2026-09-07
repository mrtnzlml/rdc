// The two environment tables give four of their columns a fixed width
// (`_Cols` in home_page.dart) because their content is bounded — an org id,
// a file count, `_rel`'s output, and the widest status word — and let only
// Environment/Connection and Host flex, since only those hold
// arbitrary-length strings.
//
// Before that rebalance the action cluster took flex 4 against the info
// columns' 8, handing four 26px icon buttons a third of the whole table (a
// measured 267.7px at the default 1180px window, 354.3px at 1440px) while
// six info columns split what was left. The status badge got 29.9px of it
// and wrapped `synced` onto three lines; `…d ago` also took three, the org
// id two, and four of the seven headers two. `_MiniBadge` additionally had
// no `maxLines`/`softWrap` guard at all, unlike its sibling `_StatusPill`.
//
// WHAT THESE TESTS CAN AND CANNOT SEE. `_Cols` is sized in the real font
// (TextPainter measurements from the running app, recorded per constant),
// whose glyphs average ~0.55em against the ~1.0248em of the font
// `flutter test` substitutes. Sizing the columns for the test font instead
// would have cost ~124px of real table width, taken straight out of
// Environment and Host, to make a test convenient — so these cells DO
// truncate here, and asserting absolute fit is not available. Absolute fit
// rests on the measurements in `_Cols`.
//
// What is asserted instead is every property that does not depend on the
// font: nothing WRAPS (the reported bug, and the thing `maxLines: 1` now
// makes structurally impossible — these cases fail the moment someone
// removes it), every row is the same height, and no width throws a
// RenderFlex overflow.
//
// The widths straddle `_Cols.minTightEnv`, so both layout regimes are
// covered: 850 and 900 fall back to sharing every column by flex, 1080 and
// up give the bounded columns their fixed widths.
//
// They start at 850 rather than lower because BELOW ~800px two header bars
// overflow — `_ProjectBar`'s Row (home_page.dart:1185, by 20px at 700px)
// and `_FleetView`'s title Row (:1873, by 103px at 700px and 2.8px at
// 800px), whose title Column is not in an Expanded. Both are the
// pre-existing unguarded-rigid-Row defect that `_ConnBar` was already fixed
// for, neither is in a table, and the table assertions here pass at those
// widths too. Reaching lower would only fold an unrelated bug into this
// file's expectations.
//
// Falsification performed by hand while writing this test, transcript in the
// commit message. Removing `_MiniBadge`'s `maxLines: 1, softWrap: false`
// failed every width with `"synced"/"syncing"/"watching" is 30.0px tall vs
// 15.0px for one line`, and restoring it passed again.
//
// An earlier draft of this change made the table scroll horizontally below
// the threshold instead of falling back to flex. It is worth recording why
// that went: a scrolled table pushed the whole action cluster off-screen at
// an 800px window, and `project_view_test.dart` caught it — the Edit button
// could no longer be tapped. Nothing should become unreachable because a
// window got narrow, so the fallback shares by flex, which is also how the
// table behaved before this change.
import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/home_page.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:desktop/src/watch_state.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

const _folder = '/tmp/Rossum/acme-invoices';
const _envs = ['alpha', 'bravo', 'delta', 'echo', 'kilo'];

EnvSummary _e(String name, {int? daysAgo}) => EnvSummary(
      name: name,
      apiBase: 'https://acme.rossum.app/api/v1',
      orgId: BigInt.from(123456),
      authKind: AuthKind.token,
      lastSyncUnix: daysAgo == null
          ? null
          : DateTime.now().subtract(Duration(days: daysAgo)).millisecondsSinceEpoch ~/ 1000,
      fileCount: BigInt.from(1284),
    );

/// One env per `_St`, so every label `_badgeFor` can produce gets measured
/// and none escapes. The watched env is also the one synced longest ago,
/// which gives `_rel` its widest realistic output (`999d`).
AppState _seeded() {
  final s = AppState(Settings(parentFolder: '/tmp/Rossum'));
  s.projects = [
    ProjectItem(
        ProjectSummary(id: 'acme-invoices', name: 'acme-invoices', folder: _folder, envs: [
          _e('alpha', daysAgo: 2), // -> synced
          _e('bravo'), // lastSyncUnix null -> never
          _e('delta', daysAgo: 1), // -> syncing
          _e('echo', daysAgo: 1), // -> error
          _e('kilo', daysAgo: 999), // -> watching
        ]),
        false),
  ];
  s.selectProject(_folder);
  s.syncState[s.envKey(_folder, 'delta')] = SyncState.running;
  s.syncState[s.envKey(_folder, 'echo')] = SyncState.error;
  s.watch[s.envKey(_folder, 'kilo')] = WatchState(running: true);
  return s;
}

Future<void> _pump(WidgetTester t, double width, NavView view) async {
  t.view.physicalSize = Size(width, 800);
  t.view.devicePixelRatio = 1.0;
  addTearDown(t.view.resetPhysicalSize);
  addTearDown(t.view.resetDevicePixelRatio);
  await t.pumpWidget(MaterialApp(
    debugShowCheckedModeBanner: false,
    theme: mdhTheme(Brightness.light),
    home: Scaffold(body: MdhScaffold(state: _seeded(), view: view)),
  ));
  await t.pumpAndSettle();
}

/// A laid-out box is on one line when it is no taller than the same box
/// would be with unlimited width. Comparing against the box's own intrinsic
/// height rather than a pixel constant keeps this independent of the font's
/// line height — the point of the exercise.
void _singleLine(WidgetTester t, List<String> labels, double width) {
  final bad = <String>[];
  for (final label in labels) {
    final found = find.text(label).evaluate().toList();
    expect(found, isNotEmpty, reason: '"$label" did not render at ${width}px');
    for (final e in found) {
      final ro = e.renderObject;
      if (ro is! RenderBox) continue;
      final one = ro.getMaxIntrinsicHeight(double.infinity);
      if (ro.size.height > one + 0.5) {
        bad.add('"$label" is ${ro.size.height.toStringAsFixed(1)}px tall '
            'vs ${one.toStringAsFixed(1)}px for one line');
      }
    }
  }
  expect(bad, isEmpty, reason: 'wrapped at ${width}px -> ${bad.join("; ")}');
}

/// The badge sets `softWrap: false`, so it lays out at its intrinsic width
/// whatever the column allows; a wrap would show up as extra height on the
/// badge itself, and a row that grew taller than its siblings means some
/// cell in it restacked. Both are what the old layout did.
void _rowsAreUniform(WidgetTester t, String keyPrefix, double width) {
  final heights = <String, double>{};
  for (final env in _envs) {
    final cell = find.byKey(ValueKey('$keyPrefix$env'));
    expect(cell, findsOneWidget, reason: 'no status cell for $env at ${width}px');
    final badge = find.descendant(of: cell, matching: find.byType(Container));
    final ro = t.renderObject<RenderBox>(badge);
    final one = ro.getMaxIntrinsicHeight(double.infinity);
    expect(ro.size.height, lessThanOrEqualTo(one + 0.5),
        reason: '$env badge wrapped at ${width}px: ${ro.size.height.toStringAsFixed(1)}px '
            'vs ${one.toStringAsFixed(1)}px for one line');
    heights[env] = t.getRect(cell).height;
  }
  expect(heights.values.toSet(), hasLength(1),
      reason: 'status cells differ in height at ${width}px: $heights');
}

// 1080 is the golden surface, 1180 the window's initial size, 1440 a roomy
// one; 800 and 700 sit below _Cols.minEnvTableW so the scroll guard carries
// them.
const _widths = [850.0, 900.0, 1080.0, 1180.0, 1440.0];

const _cells = ['123456', '1284', 'acme.rossum.app', '2d', '1d', '999d', '—'];
const _badges = ['synced', 'never', 'syncing', 'error', 'watching'];
const _envCells = [
  'acme-invoices · alpha',
  'acme-invoices · bravo',
  'acme-invoices · delta',
  'acme-invoices · echo',
  'acme-invoices · kilo',
];

void main() {
  group('project view env table', () {
    const headers = ['ENVIRONMENT', 'ORG', 'HOST', 'FILES', 'LAST SYNC', 'STATUS', 'ACTIONS'];
    for (final w in _widths) {
      testWidgets('lays out on single lines at ${w.toInt()}px', (t) async {
        await _pump(t, w, NavView.connection);
        _singleLine(t, [...headers, ..._cells, ..._badges, ..._envCells], w);
        _rowsAreUniform(t, 'status-cell-', w);
        expect(t.takeException(), isNull, reason: 'layout threw at ${w}px');
      });
    }
  });

  // Pins both regimes by the property that distinguishes them, rather than
  // by a pixel constant: a FIXED column does not grow when the window does,
  // and a FLEXED one does. Stated that way this survives any retuning of
  // the widths in _Cols, and still fails if a regime is dropped — delete
  // the tight branch and the first expectation sees the column grow.
  testWidgets('bounded columns are fixed above the threshold, flexed below', (t) async {
    Future<double> statusWidth(double w) async {
      await _pump(t, w, NavView.connection);
      return t.getRect(find.byKey(const ValueKey('status-cell-alpha'))).width;
    }

    // Both over _Cols.minTightEnv: the status column must not have moved.
    expect(await statusWidth(1180), await statusWidth(1440),
        reason: 'above the threshold the status column is a fixed width, so '
            'widening the window must not widen it');

    // Both under it: now it shares the table by flex, so it must have.
    expect(await statusWidth(850), lessThan(await statusWidth(900)),
        reason: 'below the threshold the status column flexes, so widening '
            'the window must widen it');
  });

  group('fleet view table', () {
    // The same columns minus the action cluster, so it has to agree on the
    // widths — a divergence here means the two tables drifted apart.
    const headers = ['CONNECTION', 'ORG', 'HOST', 'FILES', 'LAST SYNC', 'STATUS'];
    for (final w in _widths) {
      testWidgets('lays out on single lines at ${w.toInt()}px', (t) async {
        await _pump(t, w, NavView.overview);
        _singleLine(t, [...headers, ..._cells, ..._badges, ..._envCells], w);
        _rowsAreUniform(t, 'fleet-status-cell-', w);
        expect(t.takeException(), isNull, reason: 'layout threw at ${w}px');
      });
    }
  });
}
