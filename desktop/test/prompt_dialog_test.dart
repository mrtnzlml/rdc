import 'package:desktop/src/dialogs.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/watch_state.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

PendingPrompt _conflict() => PendingPrompt(
      id: BigInt.one,
      kind: PromptKindDto.conflict,
      question: '[k] keep local  [r] use dev  [s] skip (shadow file)  [a] abort > ',
      // What the bridge offers after stripping [e] and [h].
      keys: [
        PromptChoice(key: 'k', label: 'keep local'),
        PromptChoice(key: 'r', label: 'use dev'),
        PromptChoice(key: 's', label: 'skip (shadow file)'),
        PromptChoice(key: 'a', label: 'abort'),
      ],
      folder: '/tmp/acme',
      env: 'dev',
    );

void main() {
  testWidgets('offers one button per key and reports the key pressed', (t) async {
    String? answered;
    await t.pumpWidget(MaterialApp(
      theme: mdhTheme(Brightness.light),
      home: Scaffold(
        body: PromptDialog(
          prompt: _conflict(),
          logTail: ['patch  queues  invoices  +2  -1'],
          onAnswer: (k) => answered = k,
        ),
      ),
    ));

    // Exact match, not `textContaining`: the raw `prompt.question` fixture
    // above restates every key on one line (as the CLI printed it), so a
    // substring search matches both that line and the button — only an
    // exact match isolates the button itself.
    expect(find.text('[k] keep local'), findsOneWidget);
    expect(find.text('[r] use dev'), findsOneWidget);
    // The two terminal-only keys must never reach the UI, in either the
    // buttons or the raw question line.
    expect(find.textContaining('[e]'), findsNothing);
    expect(find.textContaining('[h]'), findsNothing);

    await t.tap(find.text('[r] use dev'));
    await t.pump();
    expect(answered, 'r');
  });
}
