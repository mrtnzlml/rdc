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
