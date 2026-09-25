import 'package:desktop/src/app_state.dart';
import 'package:desktop/src/dialogs.dart';
import 'package:desktop/src/mdh_theme.dart';
import 'package:desktop/src/rust/api/rdc.dart';
import 'package:desktop/src/settings.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

const _followUp = 'rename the CI variable RDC_TOKEN_DEV to RDC_TOKEN_SANDBOX';
const _warning = '.gitlab-ci.yml line 3: still names "dev" outside the rdc regions; rdc left it alone';

final _env = EnvSummary(
    name: 'dev',
    apiBase: 'https://x.test/api/v1',
    orgId: BigInt.one,
    authKind: AuthKind.token,
    lastSyncUnix: null,
    fileCount: BigInt.zero);
final _item = ProjectItem(ProjectSummary(id: '/tmp/acme', name: 'acme', folder: '/tmp/acme', envs: [_env]), false);

/// Stands in for the bridge: the dialog only ever calls [editEnvEntry].
class _StubState extends AppState {
  _StubState(this.result) : super(Settings(parentFolder: '/tmp'));
  final Object result; // RenameNotes to return, or an error to throw

  @override
  Future<RenameNotes> editEnvEntry(ProjectItem item, EnvSummary env, EditConnectionInput input,
      {String? newEnvName}) async {
    final r = result;
    if (r is RenameNotes) return r;
    throw r;
  }
}

Future<void> _submitDialog(WidgetTester t, AppState state) async {
  await t.pumpWidget(MaterialApp(
    theme: mdhTheme(Brightness.light),
    home: Scaffold(
      body: Builder(
        builder: (context) => TextButton(
          onPressed: () => showDialog<bool>(
              context: context, builder: (_) => EditConnectionDialog(state: state, item: _item, env: _env)),
          child: const Text('open'),
        ),
      ),
    ),
  ));
  await t.tap(find.text('open'));
  await t.pumpAndSettle();
  await t.tap(find.text('Save'));
  await t.pumpAndSettle();
}

void main() {
  test('renameNotesMessage is null without notes', () {
    expect(renameNotesMessage(const RenameNotes()), isNull);
  });

  test('renameNotesMessage keeps warnings apart from the to-do list', () {
    expect(
      renameNotesMessage(const RenameNotes(warnings: [_warning], followUps: [_followUp])),
      'Renamed.\nStill to do:\n• $_followUp\nWarnings:\n• $_warning',
    );
    expect(renameNotesMessage(const RenameNotes(followUps: [_followUp])), 'Renamed.\nStill to do:\n• $_followUp');
  });

  test('a failure after the rename still carries its follow-ups', () {
    final e = RenamedButFailed(Exception('saving the connection failed'),
        const RenameNotes(followUps: [_followUp]));
    expect(e.toString(), startsWith('saving the connection failed'));
    expect(e.toString(), contains('The environment was renamed.'));
    expect(e.toString(), contains(_followUp));
  });

  testWidgets('the edit dialog shows the rename follow-ups in a snackbar', (t) async {
    await _submitDialog(t, _StubState(const RenameNotes(followUps: [_followUp])));
    expect(find.byType(EditConnectionDialog), findsNothing);
    expect(find.textContaining(_followUp), findsOneWidget);
  });

  testWidgets('the edit dialog keeps the follow-ups when a later step fails', (t) async {
    await _submitDialog(
        t,
        _StubState(RenamedButFailed(
            Exception('saving the connection failed'), const RenameNotes(followUps: [_followUp]))));
    expect(find.byType(EditConnectionDialog), findsOneWidget);
    expect(find.textContaining(_followUp), findsOneWidget);
  });
}
