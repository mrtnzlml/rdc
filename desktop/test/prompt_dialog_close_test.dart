// Regression coverage for the gap where nothing closed `PromptDialog` when
// its prompt resolved some way other than the user pressing a button:
// `_promptOpen` was set when the dialog was scheduled and cleared only
// after `showDialog`'s future completed, which only happened via
// `onAnswer`. A watch stopping, a stream erroring, or a `SyncPhase.error`
// clearing `pendingPrompts` all left the modal on screen forever, with no
// Cancel button and `barrierDismissible: false`.
import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/home_page.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:desktop/src/watch_state.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

const _folder = '/tmp/acme';
const _env = 'dev';

PendingPrompt _prompt(int id) => PendingPrompt(
      id: BigInt.from(id),
      kind: PromptKindDto.deleteGate,
      question: 'Proceed with deletion? [y/N] ',
      keys: [
        PromptChoice(key: 'y', label: 'delete them'),
        PromptChoice(key: 'n', label: 'cancel'),
      ],
      folder: _folder,
      env: _env,
    );

// `parentFolder: null` keeps `HomePage.initState` from calling
// `state.reload()`, which would make a real (unavailable, in `flutter
// test`) Rust bridge call — see `MdhScaffold`'s onboarding-empty-state
// branch, which is all this renders behind the dialog.
AppState _headlessState() => AppState(Settings(parentFolder: null));

Future<void> _pumpHome(WidgetTester t, AppState s) => t.pumpWidget(
      MaterialApp(theme: mdhTheme(Brightness.light), home: HomePage(state: s)),
    );

void main() {
  testWidgets('answering the prompt closes the dialog, and does not leave _promptOpen stuck', (t) async {
    final s = _headlessState();
    final k = s.envKey(_folder, _env);
    s.pendingPrompts[k] = _prompt(1);

    await _pumpHome(t, s);
    await t.pumpAndSettle();
    expect(find.text('[y] delete them'), findsOneWidget);

    await t.tap(find.text('[y] delete them'));
    // `onAnswer` clears `_openPromptKey`/`_openPromptId` FIRST (see its own
    // comment), then calls `state.answer`, which calls the real
    // `answerPrompt` bridge function -- unavailable under `flutter test`
    // (no native library loaded), so it throws before ever reaching the
    // explicit `Navigator.pop()` on the next line. That throw is an
    // artifact of this test environment, not of the fix under test:
    // production's `answerPrompt` doesn't throw, so the explicit pop
    // always runs right after. Consume the expected exception here.
    expect(t.takeException(), isA<StateError>());
    await t.pump();

    // Proof the clear-before-call ordering doesn't make the
    // "resolved-externally" branch race the explicit pop: with
    // `_openPromptKey` already null, that branch has nothing to act on, so
    // the dialog is untouched until something actually pops it.
    expect(find.text('[y] delete them'), findsOneWidget);

    // Finish what `state.answer` does after a bridge call that (in
    // production) succeeds instead of throwing.
    s.pendingPrompts.remove(k);
    s.notifyListeners();
    await t.pump();
    Navigator.of(t.element(find.text('[y] delete them'))).pop();
    await t.pumpAndSettle();

    expect(find.text('[y] delete them'), findsNothing);

    // `_promptOpen` must not be left stuck true: a fresh prompt has to be
    // able to open right after.
    s.pendingPrompts[k] = _prompt(2);
    s.notifyListeners();
    await t.pumpAndSettle();
    expect(find.text('[y] delete them'), findsOneWidget);
  });

  testWidgets('the dialog closes itself when the watch is stopped (PromptResolved, no answer)', (t) async {
    final s = _headlessState();
    final k = s.envKey(_folder, _env);
    s.pendingPrompts[k] = _prompt(1);

    await _pumpHome(t, s);
    await t.pumpAndSettle();
    expect(find.text('[y] delete them'), findsOneWidget);

    // What both `SyncPhase_PromptResolved` arms call: `ask()` returning
    // `None` because the cancel token fired while the prompt was parked —
    // exactly what a stopped watch looks like. Nobody clicked a button.
    s.resolvePrompt(k, BigInt.from(1));
    s.notifyListeners();
    await t.pumpAndSettle();

    expect(
      find.text('[y] delete them'),
      findsNothing,
      reason: 'nothing else closes this dialog; it must close itself',
    );

    // `_promptOpen` must not be left stuck true: a fresh prompt (even for
    // the same env) has to be able to open right after.
    s.pendingPrompts[k] = _prompt(2);
    s.notifyListeners();
    await t.pumpAndSettle();
    expect(find.text('[y] delete them'), findsOneWidget);
  });

  testWidgets('the dialog closes itself when the watch stream errors (SyncPhase.error)', (t) async {
    final s = _headlessState();
    final k = s.envKey(_folder, _env);
    s.watch[k] = WatchState(running: true);
    s.pendingPrompts[k] = _prompt(1);

    await _pumpHome(t, s);
    await t.pumpAndSettle();
    expect(find.text('[y] delete them'), findsOneWidget);

    // What `applyWatchPhase`'s `SyncPhase_Error` arm does: clears
    // `pendingPrompts[k]` directly, with no `PromptResolved` in between.
    s.applyWatchPhase(_folder, _env, const SyncPhase.error(message: 'network blip'));
    await t.pumpAndSettle();

    expect(find.text('[y] delete them'), findsNothing);
  });

  testWidgets('a different env resolving does not close the dialog on screen', (t) async {
    final s = _headlessState();
    final kShown = s.envKey(_folder, _env);
    const otherEnv = 'prod';
    final kOther = s.envKey(_folder, otherEnv);
    s.pendingPrompts[kShown] = _prompt(1);
    s.pendingPrompts[kOther] = PendingPrompt(
      id: BigInt.from(2),
      kind: PromptKindDto.deleteGate,
      question: 'Proceed with deletion? [y/N] ',
      keys: [PromptChoice(key: 'y', label: 'delete them'), PromptChoice(key: 'n', label: 'cancel')],
      folder: _folder,
      env: otherEnv,
    );

    await _pumpHome(t, s);
    await t.pumpAndSettle();
    // The dev prompt (queued first) is the one on screen.
    expect(find.text('[y] delete them'), findsOneWidget);

    // The OTHER env's prompt resolving must not touch the dialog currently
    // showing dev's.
    s.pendingPrompts.remove(kOther);
    s.notifyListeners();
    await t.pumpAndSettle();
    expect(find.text('[y] delete them'), findsOneWidget, reason: 'dev\'s dialog is unrelated to prod resolving');
  });
}
