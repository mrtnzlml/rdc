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

ProjectItem _p(String folder, List<EnvSummary> envs) => ProjectItem(
      ProjectSummary(id: folder, name: folder.split('/').last, folder: folder, envs: envs),
      false,
    );

void main() {
  group('AppState promote state machine (pure transitions, no bridge)', () {
    test('fresh AppState starts idle with the documented defaults', () {
      final s = AppState(Settings(parentFolder: '/tmp'));
      expect(s.promoteStage, PromoteStage.idle);
      expect(s.promotePolicy, ConflictPolicy.keepTarget);
      expect(s.promoteMirror, isFalse);
      expect(s.promoteAllowDeletes, isFalse);
      expect(s.promoteSrc, isNull);
      expect(s.promoteTgt, isNull);
      expect(s.promotePreview, isNull);
      expect(s.promoteLog, isEmpty);
      expect(s.promoteError, isNull);
    });

    test('setPromoteDir sets src/tgt and notifies', () {
      final s = AppState(Settings(parentFolder: '/tmp'));
      var notified = 0;
      s.addListener(() => notified++);

      s.setPromoteDir('dev', 'prod');

      expect(s.promoteSrc, 'dev');
      expect(s.promoteTgt, 'prod');
      expect(notified, greaterThan(0));
    });

    test('swapPromoteDir flips src and tgt', () {
      final s = AppState(Settings(parentFolder: '/tmp'));
      s.setPromoteDir('dev', 'prod');

      s.swapPromoteDir();

      expect(s.promoteSrc, 'prod');
      expect(s.promoteTgt, 'dev');
    });

    test('resetPromote returns to idle, clears preview/log/error but keeps direction', () {
      final s = AppState(Settings(parentFolder: '/tmp'));
      s.setPromoteDir('dev', 'prod');
      s.promoteStage = PromoteStage.preview;
      s.promotePreview = const PromotionPreview(plan: ['+ queue acme/invoices']);
      s.promoteLog = ['line 1', 'line 2'];
      s.promoteError = 'boom';

      s.resetPromote();

      expect(s.promoteStage, PromoteStage.idle);
      expect(s.promotePreview, isNull);
      expect(s.promoteLog, isEmpty);
      expect(s.promoteError, isNull);
      // Cancel just abandons the preview; it shouldn't forget the picked
      // direction, otherwise re-Preparing forces the user to re-pick.
      expect(s.promoteSrc, 'dev');
      expect(s.promoteTgt, 'prod');
    });

    test('switching to a different project resets a prepared promote (no stale preview/push)', () {
      final s = AppState(Settings(parentFolder: '/tmp'));
      s.projects = [
        _p('/tmp/acme', [_e('dev', 1), _e('prod', 2)]),
        _p('/tmp/beta', [_e('dev', 3), _e('prod', 4)]),
      ];

      // Prepare project A (acme) up to the preview stage.
      s.selectProject('/tmp/acme');
      s.setPromoteDir('dev', 'prod');
      s.promoteStage = PromoteStage.preview;
      s.promotePreview = const PromotionPreview(plan: ['+ queue acme/invoices']);

      // Switching to a different project (beta, which happens to have envs
      // of the same names) must not leave acme's stale preview/direction
      // behind — otherwise the Promote panel would render beta's Push
      // button wired to acme's reviewed plan.
      s.selectProject('/tmp/beta');

      expect(s.promoteStage, PromoteStage.idle);
      expect(s.promotePreview, isNull);
      expect(s.promoteSrc, isNull);
      expect(s.promoteTgt, isNull);
    });

    test('selecting an env within the same project does not reset a prepared promote', () {
      final s = AppState(Settings(parentFolder: '/tmp'));
      s.projects = [_p('/tmp/acme', [_e('dev', 1), _e('prod', 2)])];

      s.selectProject('/tmp/acme');
      s.setPromoteDir('dev', 'prod');
      s.promoteStage = PromoteStage.preview;
      s.promotePreview = const PromotionPreview(plan: ['+ queue acme/invoices']);

      // Navigating to an env child of the SAME project (project <-> env
      // navigation) must preserve an in-progress promote.
      s.selectEnv('/tmp/acme', 'prod');

      expect(s.promoteStage, PromoteStage.preview);
      expect(s.promotePreview, isNotNull);
      expect(s.promoteSrc, 'dev');
      expect(s.promoteTgt, 'prod');
    });
  });

  group('Promote panel (Project view)', () {
    testWidgets('renders From/To pickers + Prepare button for a >=2-env project', (t) async {
      final s = AppState(Settings(parentFolder: '/tmp'));
      s.projects = [_p('/tmp/acme', [_e('dev', 1), _e('prod', 2)])];
      s.selectProject('/tmp/acme');

      await t.pumpWidget(MaterialApp(
        theme: mdhTheme(Brightness.light),
        home: Scaffold(body: MdhScaffold(state: s, view: NavView.connection)),
      ));
      await t.pumpAndSettle();

      // _SectionTitle upper-cases its label at render time.
      expect(find.text('PROMOTE'), findsOneWidget);
      expect(find.text('From'), findsOneWidget);
      expect(find.text('To'), findsOneWidget);
      expect(find.byType(DropdownButton<String>), findsNWidgets(2));
      expect(find.textContaining('Prepare'), findsOneWidget);
      expect(find.text('Mirror'), findsOneWidget);

      // Defaults to the first two envs (src = first, tgt = second) so
      // Prepare is immediately actionable without forcing a manual pick.
      expect(s.promoteSrc, 'dev');
      expect(s.promoteTgt, 'prod');
    });

    testWidgets('absent for a single-env project', (t) async {
      final s = AppState(Settings(parentFolder: '/tmp'));
      s.projects = [_p('/tmp/acme', [_e('dev', 1)])];
      s.selectProject('/tmp/acme');

      await t.pumpWidget(MaterialApp(
        theme: mdhTheme(Brightness.light),
        home: Scaffold(body: MdhScaffold(state: s, view: NavView.connection)),
      ));
      await t.pumpAndSettle();

      expect(find.text('PROMOTE'), findsNothing);
      expect(find.byType(DropdownButton<String>), findsNothing);
    });

    testWidgets('swap button flips the selected direction', (t) async {
      final s = AppState(Settings(parentFolder: '/tmp'));
      s.projects = [_p('/tmp/acme', [_e('dev', 1), _e('prod', 2)])];
      s.selectProject('/tmp/acme');

      await t.pumpWidget(MaterialApp(
        theme: mdhTheme(Brightness.light),
        home: Scaffold(body: MdhScaffold(state: s, view: NavView.connection)),
      ));
      await t.pumpAndSettle();

      expect(s.promoteSrc, 'dev');
      expect(s.promoteTgt, 'prod');

      await t.tap(find.byTooltip('Swap direction'));
      await t.pump();

      expect(s.promoteSrc, 'prod');
      expect(s.promoteTgt, 'dev');
    });
  });
}
