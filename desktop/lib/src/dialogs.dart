import 'package:flutter/material.dart';

import 'ansi.dart';
import 'app_state.dart';
import 'error_text.dart';
import 'mdh_theme.dart';
import 'rust/api/rdc.dart';
import 'watch_state.dart';

// ------------------------------------------------------------ shared shell

/// MDH-style modal: a clean card with a title, body, and a right-aligned
/// action row. By default that row is Cancel + one primary action (every
/// dialog below except [PromptDialog] uses this). Pass [actions] to render a
/// different row **in place of** that default footer instead — [PromptDialog]
/// needs this because its answer set is N keys the core already chose, none
/// of which is a generic "Cancel", and the default Cancel button pops the
/// dialog via `Navigator.pop` **without calling any handler** — exactly
/// wrong for a dialog whose whole point is that a worker thread is parked
/// behind it waiting for one specific key to come back.
class _Frame extends StatelessWidget {
  const _Frame({
    required this.title,
    required this.child,
    this.primaryLabel,
    this.onPrimary,
    this.busy = false,
    this.actions,
    this.maxWidth = 440,
  }) : assert(actions != null || primaryLabel != null, 'either actions, or primaryLabel/onPrimary, is required');
  final String title;
  final Widget child;
  final String? primaryLabel;
  final VoidCallback? onPrimary;
  final bool busy;
  final List<Widget>? actions;
  final double maxWidth;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Dialog(
      backgroundColor: c.bgCard,
      surfaceTintColor: Colors.transparent,
      shape: RoundedRectangleBorder(side: BorderSide(color: c.border), borderRadius: BorderRadius.circular(10)),
      child: ConstrainedBox(
        constraints: BoxConstraints(maxWidth: maxWidth),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 18, 20, 6),
              child: Align(
                alignment: Alignment.centerLeft,
                child: Text(title, style: TextStyle(color: c.textPrimary, fontSize: 16, fontWeight: FontWeight.w600)),
              ),
            ),
            Flexible(child: SingleChildScrollView(padding: const EdgeInsets.fromLTRB(20, 8, 20, 8), child: child)),
            Padding(
              padding: const EdgeInsets.fromLTRB(16, 8, 16, 14),
              child: actions != null
                  ? Wrap(alignment: WrapAlignment.end, spacing: 8, runSpacing: 8, children: actions!)
                  : Row(children: [
                      const Spacer(),
                      TextButton(onPressed: () => Navigator.of(context).pop(false), child: const Text('Cancel')),
                      const SizedBox(width: 8),
                      FilledButton(
                        onPressed: busy ? null : onPrimary,
                        child: busy
                            ? const SizedBox(width: 15, height: 15, child: CircularProgressIndicator(strokeWidth: 2))
                            : Text(primaryLabel!),
                      ),
                    ]),
            ),
          ],
        ),
      ),
    );
  }
}

class _Field extends StatelessWidget {
  const _Field({
    required this.label,
    required this.controller,
    this.obscure = false,
    this.hint,
    this.keyboardType,
    this.autofocus = false,
  });
  final String label;
  final TextEditingController controller;
  final bool obscure;
  final String? hint;
  final TextInputType? keyboardType;
  final bool autofocus;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Padding(
      padding: const EdgeInsets.only(bottom: 12),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(label, style: TextStyle(color: c.textSecondary, fontSize: 11.5, fontWeight: FontWeight.w600, letterSpacing: 0.3)),
          const SizedBox(height: 6),
          TextField(
            controller: controller,
            autofocus: autofocus,
            obscureText: obscure,
            keyboardType: keyboardType,
            style: TextStyle(color: c.textPrimary, fontSize: 13),
            cursorColor: c.accent,
            decoration: InputDecoration(
              isDense: true,
              hintText: hint,
              hintStyle: TextStyle(color: c.textHint, fontSize: 13),
              contentPadding: const EdgeInsets.symmetric(horizontal: 10, vertical: 9),
              filled: true,
              fillColor: c.bgBase,
              enabledBorder: OutlineInputBorder(borderRadius: BorderRadius.circular(6), borderSide: BorderSide(color: c.border)),
              focusedBorder: OutlineInputBorder(borderRadius: BorderRadius.circular(6), borderSide: BorderSide(color: c.accent)),
            ),
          ),
        ],
      ),
    );
  }
}

class _AuthToggle extends StatelessWidget {
  const _AuthToggle({required this.value, required this.onChanged});
  final AuthKind value;
  final ValueChanged<AuthKind> onChanged;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    Widget opt(AuthKind k, String label) {
      final sel = k == value;
      return Expanded(
        child: InkWell(
          onTap: () => onChanged(k),
          borderRadius: BorderRadius.circular(6),
          child: Container(
            alignment: Alignment.center,
            padding: const EdgeInsets.symmetric(vertical: 8),
            decoration: BoxDecoration(
              color: sel ? c.accent : c.bgBase,
              border: Border.all(color: sel ? c.accent : c.border),
              borderRadius: BorderRadius.circular(6),
            ),
            child: Text(label, style: TextStyle(color: sel ? Colors.white : c.textSecondary, fontSize: 12.5, fontWeight: FontWeight.w600)),
          ),
        ),
      );
    }

    return Padding(
      padding: const EdgeInsets.only(bottom: 12),
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Text('AUTHENTICATION', style: TextStyle(color: c.textSecondary, fontSize: 11.5, fontWeight: FontWeight.w600, letterSpacing: 0.3)),
        const SizedBox(height: 6),
        Row(children: [opt(AuthKind.token, 'API token'), const SizedBox(width: 8), opt(AuthKind.password, 'Username & password')]),
      ]),
    );
  }
}

class _ErrLine extends StatelessWidget {
  const _ErrLine(this.text);
  final String text;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Container(
      margin: const EdgeInsets.only(top: 4),
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
      decoration: BoxDecoration(color: c.dangerBg, border: Border.all(color: c.dangerBorder), borderRadius: BorderRadius.circular(6)),
      child: SelectableText(text, style: TextStyle(color: c.dangerFg, fontSize: 12)),
    );
  }
}

// ------------------------------------------------------------ add

class AddConnectionDialog extends StatefulWidget {
  const AddConnectionDialog({super.key, required this.state});
  final AppState state;
  @override
  State<AddConnectionDialog> createState() => _AddConnectionDialogState();
}

class _AddConnectionDialogState extends State<AddConnectionDialog> {
  final _name = TextEditingController();
  final _envName = TextEditingController();
  final _apiBase = TextEditingController(text: 'https://<org>.rossum.app/api/v1');
  final _orgId = TextEditingController();
  final _token = TextEditingController();
  final _username = TextEditingController();
  final _password = TextEditingController();
  AuthKind _auth = AuthKind.token;
  String? _error;
  bool _busy = false;

  @override
  void dispose() {
    for (final c in [_name, _envName, _apiBase, _orgId, _token, _username, _password]) {
      c.dispose();
    }
    super.dispose();
  }

  Future<void> _submit() async {
    final org = BigInt.tryParse(_orgId.text.trim());
    if (_name.text.trim().isEmpty) return setState(() => _error = 'Name is required.');
    if (_envName.text.trim().isEmpty) return setState(() => _error = 'Environment name is required.');
    if (org == null) return setState(() => _error = 'Organization ID must be a number.');
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      await widget.state.addProjectEntry(
        _name.text.trim(),
        AddEnvInput(
          name: _envName.text.trim(),
          apiBase: _apiBase.text.trim(),
          orgId: org,
          authKind: _auth,
          token: _auth == AuthKind.token ? _token.text : null,
          username: _auth == AuthKind.password ? _username.text : null,
          password: _auth == AuthKind.password ? _password.text : null,
        ),
      );
      if (mounted) Navigator.of(context).pop(true);
    } catch (e) {
      setState(() {
        _busy = false;
        _error = errorText(e);
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    return _Frame(
      title: 'New project',
      primaryLabel: 'Create',
      busy: _busy,
      onPrimary: _submit,
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        _Field(label: 'PROJECT NAME', controller: _name, autofocus: true),
        _Field(label: 'ENVIRONMENT NAME', controller: _envName, hint: 'e.g. prod'),
        _Field(label: 'API BASE URL', controller: _apiBase),
        _Field(label: 'ORGANIZATION ID', controller: _orgId, keyboardType: TextInputType.number),
        _AuthToggle(value: _auth, onChanged: (a) => setState(() => _auth = a)),
        if (_auth == AuthKind.token)
          _Field(label: 'API TOKEN', controller: _token, obscure: true)
        else ...[
          _Field(label: 'USERNAME', controller: _username),
          _Field(label: 'PASSWORD', controller: _password, obscure: true),
        ],
        if (_error != null) _ErrLine(_error!),
      ]),
    );
  }
}

// ------------------------------------------------------------ add env

class AddEnvDialog extends StatefulWidget {
  const AddEnvDialog({super.key, required this.state, required this.item});
  final AppState state;
  final ProjectItem item;
  @override
  State<AddEnvDialog> createState() => _AddEnvDialogState();
}

class _AddEnvDialogState extends State<AddEnvDialog> {
  final _envName = TextEditingController();
  final _apiBase = TextEditingController(text: 'https://<org>.rossum.app/api/v1');
  final _orgId = TextEditingController();
  final _token = TextEditingController();
  final _username = TextEditingController();
  final _password = TextEditingController();
  AuthKind _auth = AuthKind.token;
  String? _error;
  bool _busy = false;

  @override
  void dispose() {
    for (final c in [_envName, _apiBase, _orgId, _token, _username, _password]) {
      c.dispose();
    }
    super.dispose();
  }

  Future<void> _submit() async {
    final org = BigInt.tryParse(_orgId.text.trim());
    if (_envName.text.trim().isEmpty) return setState(() => _error = 'Environment name is required.');
    if (org == null) return setState(() => _error = 'Organization ID must be a number.');
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      await widget.state.addEnvEntry(
        widget.item,
        AddEnvInput(
          name: _envName.text.trim(),
          apiBase: _apiBase.text.trim(),
          orgId: org,
          authKind: _auth,
          token: _auth == AuthKind.token ? _token.text : null,
          username: _auth == AuthKind.password ? _username.text : null,
          password: _auth == AuthKind.password ? _password.text : null,
        ),
      );
      if (mounted) Navigator.of(context).pop(true);
    } catch (e) {
      setState(() {
        _busy = false;
        _error = errorText(e);
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    return _Frame(
      title: 'Add environment',
      primaryLabel: 'Add',
      busy: _busy,
      onPrimary: _submit,
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        _Field(label: 'ENVIRONMENT NAME', controller: _envName, autofocus: true, hint: 'e.g. prod'),
        _Field(label: 'API BASE URL', controller: _apiBase),
        _Field(label: 'ORGANIZATION ID', controller: _orgId, keyboardType: TextInputType.number),
        _AuthToggle(value: _auth, onChanged: (a) => setState(() => _auth = a)),
        if (_auth == AuthKind.token)
          _Field(label: 'API TOKEN', controller: _token, obscure: true)
        else ...[
          _Field(label: 'USERNAME', controller: _username),
          _Field(label: 'PASSWORD', controller: _password, obscure: true),
        ],
        if (_error != null) _ErrLine(_error!),
      ]),
    );
  }
}

// ------------------------------------------------------------ edit

class EditConnectionDialog extends StatefulWidget {
  const EditConnectionDialog({super.key, required this.state, required this.item, required this.env});
  final AppState state;
  final ProjectItem item;
  final EnvSummary env;
  @override
  State<EditConnectionDialog> createState() => _EditConnectionDialogState();
}

class _EditConnectionDialogState extends State<EditConnectionDialog> {
  late final _name = TextEditingController(text: widget.item.summary.name);
  late final _envName = TextEditingController(text: widget.env.name);
  late final _apiBase = TextEditingController(text: widget.env.apiBase);
  late final _orgId = TextEditingController(text: widget.env.orgId.toString());
  final _token = TextEditingController();
  final _username = TextEditingController();
  final _password = TextEditingController();
  late AuthKind _auth = widget.env.authKind;
  String? _error;
  bool _busy = false;

  @override
  void dispose() {
    for (final c in [_name, _envName, _apiBase, _orgId, _token, _username, _password]) {
      c.dispose();
    }
    super.dispose();
  }

  Future<void> _submit() async {
    final org = BigInt.tryParse(_orgId.text.trim());
    if (_name.text.trim().isEmpty) return setState(() => _error = 'Name is required.');
    if (_envName.text.trim().isEmpty) return setState(() => _error = 'Environment name is required.');
    if (org == null) return setState(() => _error = 'Organization ID must be a number.');
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      await widget.state.editEnvEntry(
        widget.item,
        widget.env,
        EditConnectionInput(
          name: _name.text.trim(),
          apiBase: _apiBase.text.trim(),
          orgId: org,
          authKind: _auth,
          token: _auth == AuthKind.token ? _token.text : null,
          username: _auth == AuthKind.password ? _username.text : null,
          password: _auth == AuthKind.password ? _password.text : null,
        ),
        newEnvName: _envName.text.trim(),
      );
      if (mounted) Navigator.of(context).pop(true);
    } catch (e) {
      setState(() {
        _busy = false;
        _error = errorText(e);
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return _Frame(
      title: 'Edit environment',
      primaryLabel: 'Save',
      busy: _busy,
      onPrimary: _submit,
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        _Field(label: 'PROJECT NAME', controller: _name, autofocus: true),
        _Field(label: 'ENVIRONMENT NAME', controller: _envName, hint: 'e.g. prod'),
        _Field(label: 'API BASE URL', controller: _apiBase),
        _Field(label: 'ORGANIZATION ID', controller: _orgId, keyboardType: TextInputType.number),
        _AuthToggle(value: _auth, onChanged: (a) => setState(() => _auth = a)),
        if (_auth == AuthKind.token)
          _Field(label: 'API TOKEN', controller: _token, obscure: true, hint: 'leave blank to keep current')
        else ...[
          _Field(label: 'USERNAME', controller: _username, hint: 'leave blank to keep current'),
          _Field(label: 'PASSWORD', controller: _password, obscure: true, hint: 'leave blank to keep current'),
        ],
        Text('Leave credentials blank to keep the current ones.', style: TextStyle(color: c.textSecondary, fontSize: 11.5)),
        if (_error != null) _ErrLine(_error!),
      ]),
    );
  }
}

// ------------------------------------------------------------ remove / detach

class RemoveDialog extends StatelessWidget {
  const RemoveDialog({super.key, required this.item});
  final ProjectItem item;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final external = item.isExternal;
    return _Frame(
      title: external ? 'Detach project?' : 'Remove project?',
      primaryLabel: external ? 'Detach' : 'Move to Trash',
      onPrimary: () => Navigator.of(context).pop(true),
      child: Text(
        external
            ? 'Forgets "${item.summary.name}" from the app. The folder on disk is left untouched.'
            : 'Moves "${item.summary.name}" and its files to the Trash.',
        style: TextStyle(color: c.textPrimary, fontSize: 13, height: 1.5),
      ),
    );
  }
}

class RemoveEnvDialog extends StatelessWidget {
  const RemoveEnvDialog({super.key, required this.item, required this.env});
  final ProjectItem item;
  final EnvSummary env;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return _Frame(
      title: 'Remove environment?',
      primaryLabel: 'Remove',
      onPrimary: () => Navigator.of(context).pop(true),
      child: Text(
        'Removes the "${env.name}" environment and its local files from "${item.summary.name}". '
        'If it is the last environment, the whole project is moved to the Trash.',
        style: TextStyle(color: c.textPrimary, fontSize: 13, height: 1.5),
      ),
    );
  }
}

// ------------------------------------------------------------ blocked prompt

/// A blocked cycle, rendered. The body is the tail of the sync log — which
/// is where the diff, the connector line and the object list already are,
/// in colour — and the buttons are exactly the keys the core offered.
///
/// Built on [_Frame] like every other dialog here, but passes [actions]
/// instead of a `primaryLabel`/`onPrimary` pair: `_Frame`'s default footer
/// bakes in exactly one Cancel button (which pops the dialog without
/// answering anything) plus one primary action, but a prompt's answer set is
/// N keys chosen by the core and rendered verbatim — there is no separate
/// "Cancel" to add, and no single action to call primary. Letting `_Frame`
/// supply its default Cancel button would let the user dismiss the dialog
/// without an answer while the worker thread behind it stays parked waiting
/// for one, and would render a key ("Cancel") the core never offered.
class PromptDialog extends StatelessWidget {
  const PromptDialog({
    super.key,
    required this.prompt,
    required this.logTail,
    required this.onAnswer,
  });

  final PendingPrompt prompt;
  final List<String> logTail;
  final void Function(String key) onAnswer;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final spans = <InlineSpan>[];
    for (var i = 0; i < logTail.length; i++) {
      spans.addAll(ansiSpans(logTail[i], c, 12.5));
      if (i < logTail.length - 1) spans.add(const TextSpan(text: '\n'));
    }
    return _Frame(
      title: '${prompt.title} · ${prompt.env}',
      maxWidth: 560,
      actions: [
        for (final k in prompt.keys)
          MdhBtn(
            label: '[${k.key}] ${k.label}',
            primary: k.key == 'n' || k.key == 's',
            onTap: () => onAnswer(k.key),
          ),
      ],
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          Container(
            width: double.infinity,
            constraints: const BoxConstraints(maxHeight: 260),
            decoration: BoxDecoration(
              color: c.bgCode,
              border: Border.all(color: c.borderCard),
              borderRadius: BorderRadius.circular(6),
            ),
            padding: const EdgeInsets.fromLTRB(14, 12, 14, 12),
            child: SingleChildScrollView(
              child: SelectableText.rich(TextSpan(children: spans)),
            ),
          ),
          const SizedBox(height: 12),
          SelectableText(prompt.question, style: monoStyle(c.textSecondary, 12.5)),
        ],
      ),
    );
  }
}
