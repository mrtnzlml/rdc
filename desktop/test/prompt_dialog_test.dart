import 'package:desktop/src/dialogs.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/watch_state.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

PendingPrompt _conflict() => PendingPrompt(
      id: BigInt.one,
      kind: PromptKindDto.conflict,
      // What the bridge offers after stripping [e] and [h] — and the
      // question it rebuilds from exactly those keys, so the line never
      // names a choice this surface has no button for.
      question: '[k] keep local (push it to dev)  [r] use dev (overwrite local)'
          '  [s] decide later  [a] abort the sync > ',
      keys: [
        PromptChoice(key: 'k', label: 'keep local (push it to dev)'),
        PromptChoice(key: 'r', label: 'use dev (overwrite local)'),
        PromptChoice(key: 's', label: 'decide later'),
        PromptChoice(key: 'a', label: 'abort the sync'),
      ],
      folder: '/tmp/acme',
      env: 'dev',
    );

void main() {
  testWidgets('offers one button per key and reports the key pressed', (t) async {
    final prompt = _conflict();
    String? answered;
    await t.pumpWidget(MaterialApp(
      theme: mdhTheme(Brightness.light),
      home: Scaffold(
        body: PromptDialog(
          prompt: prompt,
          logTail: ['patch  queues  invoices  +2  -1'],
          onAnswer: (k) => answered = k,
        ),
      ),
    ));

    expect(find.text('[k] keep local (push it to dev)'), findsOneWidget);
    expect(find.text('[r] use dev (overwrite local)'), findsOneWidget);
    expect(find.text('[s] decide later'), findsOneWidget);
    expect(find.text('[a] abort the sync'), findsOneWidget);
    // One button per key, no more, no fewer — a stray extra button would
    // otherwise pass the four checks above unnoticed.
    expect(find.byType(MdhBtn), findsNWidgets(prompt.keys.length));
    // The two terminal-only keys must never reach the UI.
    expect(find.textContaining('[e]'), findsNothing);
    expect(find.textContaining('[h]'), findsNothing);
    // The menu is not repeated as prose above the buttons: `prompt.question`
    // is the same choices plus a `> ` that means nothing in a dialog.
    expect(find.text(prompt.question), findsNothing);

    await t.tap(find.text('[r] use dev (overwrite local)'));
    await t.pump();
    expect(answered, 'r');
  });

  testWidgets('answers to the letter on the button, as the terminal does', (t) async {
    final prompt = _conflict();
    String? answered;
    await t.pumpWidget(MaterialApp(
      theme: mdhTheme(Brightness.light),
      home: Scaffold(
        body: PromptDialog(
          prompt: prompt,
          logTail: const [],
          onAnswer: (k) => answered = k,
        ),
      ),
    ));

    await t.sendKeyEvent(LogicalKeyboardKey.keyR);
    await t.pump();
    expect(answered, 'r', reason: 'the [r] on the button must answer [r]');

    answered = null;
    await t.sendKeyEvent(LogicalKeyboardKey.keyA);
    await t.pump();
    expect(answered, 'a');
  });
}
