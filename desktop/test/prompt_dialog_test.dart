import 'package:desktop/src/dialogs.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/watch_state.dart';
import 'package:flutter/material.dart';
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

    // Exact match, not `textContaining`: the raw `prompt.question` fixture
    // above restates every key on one line (as the CLI printed it), so a
    // substring search matches both that line and the button — only an
    // exact match isolates the button itself.
    expect(find.text('[k] keep local (push it to dev)'), findsOneWidget);
    expect(find.text('[r] use dev (overwrite local)'), findsOneWidget);
    expect(find.text('[s] decide later'), findsOneWidget);
    expect(find.text('[a] abort the sync'), findsOneWidget);
    // One button per key, no more, no fewer — a stray extra button would
    // otherwise pass the four checks above unnoticed.
    expect(find.byType(MdhBtn), findsNWidgets(prompt.keys.length));
    // The two terminal-only keys must never reach the UI, in either the
    // buttons or the raw question line.
    expect(find.textContaining('[e]'), findsNothing);
    expect(find.textContaining('[h]'), findsNothing);

    await t.tap(find.text('[r] use dev (overwrite local)'));
    await t.pump();
    expect(answered, 'r');
  });
}
