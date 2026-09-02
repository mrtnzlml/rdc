import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:desktop/src/watch_state.dart';
import 'package:flutter_test/flutter_test.dart';

PendingPrompt _prompt(String folder, String env, int id) => PendingPrompt(
      id: BigInt.from(id),
      kind: PromptKindDto.deleteGate,
      question: 'Proceed with deletion? [y/N] ',
      keys: [
        PromptChoice(key: 'y', label: 'delete them'),
        PromptChoice(key: 'n', label: 'cancel'),
      ],
      folder: folder,
      env: env,
    );

void main() {
  test('watch state is keyed per (folder, env)', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    s.watch[s.envKey('/tmp/acme', 'dev')] = WatchState(running: true);
    expect(s.isWatching('/tmp/acme', 'dev'), isTrue);
    expect(s.isWatching('/tmp/acme', 'prod'), isFalse);
    expect(s.isWatching('/tmp/beta', 'dev'), isFalse);
  });

  test('two blocked envs both appear in the prompt queue', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    s.watch[s.envKey('/tmp/acme', 'dev')] = WatchState(running: true);
    s.watch[s.envKey('/tmp/beta', 'dev')] = WatchState(running: true);
    s.pendingPrompts[s.envKey('/tmp/acme', 'dev')] = _prompt('/tmp/acme', 'dev', 1);
    s.pendingPrompts[s.envKey('/tmp/beta', 'dev')] = _prompt('/tmp/beta', 'dev', 1);
    expect(s.promptQueue.length, 2);
  });

  test('a stale PromptResolved does not clear a newer prompt', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    final k = s.envKey('/tmp/acme', 'dev');
    s.pendingPrompts[k] = _prompt('/tmp/acme', 'dev', 7);
    // Both SyncPhase_PromptResolved arms call this directly, so driving it
    // here (rather than reimplementing the guard inline) actually exercises
    // the production code path.
    s.resolvePrompt(k, BigInt.from(6));
    expect(s.pendingPrompts[k], isNotNull);
    s.resolvePrompt(k, BigInt.from(7));
    expect(s.pendingPrompts[k], isNull);
  });

  test('a watch that ends in error clears its entry so the env is usable again', () {
    final s = AppState(Settings(parentFolder: '/tmp'));
    const folder = '/tmp/acme';
    const env = 'dev';
    final k = s.envKey(folder, env);

    // What watchEnvItem installs when a cycle starts.
    s.watch[k] = WatchState(running: true);

    // The Rust side makes Error and Stopped mutually exclusive terminal
    // outcomes of the same watch call: once an Error phase arrives, no
    // Stopped is ever coming to clear this entry. If applyWatchPhase only
    // flipped `running` to false (as it used to) instead of removing the
    // entry, `watch[k]` would sit there forever — `isWatching` already
    // reads false, but any presence-based "a watch still owns this env"
    // check (which is exactly how the desktop UI gates Sync/Watch) would
    // stay true permanently, for something as ordinary as a network blip
    // or an expired token.
    s.applyWatchPhase(folder, env, const SyncPhase.error(message: 'token expired'));

    expect(s.watch[k], isNull, reason: 'an errored watch must not leave its entry behind');
    expect(s.isWatching(folder, env), isFalse);
    expect(s.syncState[k], SyncState.error);
    expect(s.syncMessage[k], 'token expired');
  });

  test('the idle countdown ticks down once a second and stops when the watch ends', () async {
    // Regression test for the gap where `SyncPhase.idle` had a producer on
    // the Rust side but nothing on the Dart side ever advanced the number
    // it set — the sidebar showed a static "watching · 3s" forever instead
    // of a real countdown.
    final s = AppState(Settings(parentFolder: '/tmp'));
    const folder = '/tmp/acme';
    const env = 'dev';
    final k = s.envKey(folder, env);
    s.watch[k] = WatchState(running: true);

    // `Started` is what starts the ticker (see `_ensureIdleTicker`); `Idle`
    // is what sets the number it counts down from.
    s.applyWatchPhase(folder, env, const SyncPhase.started());
    s.applyWatchPhase(folder, env, SyncPhase.idle(nextPollSecs: BigInt.from(3)));
    expect(s.watch[k]!.nextPollSecs, 3);

    await Future<void>.delayed(const Duration(milliseconds: 1100));
    expect(s.watch[k]!.nextPollSecs, 2);

    await Future<void>.delayed(const Duration(seconds: 1));
    expect(s.watch[k]!.nextPollSecs, 1);

    // Ending the watch must stop the ticker, not just clear this one entry
    // — a live `Timer.periodic` outliving every watch would tick forever.
    // Uses the `error` phase (not `stopped`, which also calls `reload()` —
    // a real bridge call this plain `test()` can't make) to end it, the
    // same terminal path `a watch that ends in error clears its entry...`
    // above exercises.
    s.applyWatchPhase(folder, env, const SyncPhase.error(message: 'token expired'));
    expect(s.watch[k], isNull);
    s.dispose(); // releases the ticker if `_stopIdleTickerIfIdle` somehow missed it
  });

  test('every prompt kind has a title', () {
    for (final k in PromptKindDto.values) {
      final t = _prompt('/tmp/acme', 'dev', 1);
      expect(
        PendingPrompt(id: t.id, kind: k, question: t.question, keys: t.keys, folder: t.folder, env: t.env)
            .title
            .isNotEmpty,
        isTrue,
        reason: 'no title for $k',
      );
    }
  });
}
