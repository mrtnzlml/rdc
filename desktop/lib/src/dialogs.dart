import 'package:flutter/material.dart';

import 'app_state.dart';
import 'error_text.dart';
import 'mdh_theme.dart';
import 'rust/api/rdc.dart';

// ------------------------------------------------------------ shared shell

/// MDH-style modal: a clean card with a title, body, and a right-aligned
/// Cancel + primary action.
class _Frame extends StatelessWidget {
  const _Frame({
    required this.title,
    required this.child,
    required this.primaryLabel,
    required this.onPrimary,
    this.busy = false,
  });
  final String title;
  final Widget child;
  final String primaryLabel;
  final VoidCallback? onPrimary;
  final bool busy;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Dialog(
      backgroundColor: c.bgCard,
      surfaceTintColor: Colors.transparent,
      shape: RoundedRectangleBorder(side: BorderSide(color: c.border), borderRadius: BorderRadius.circular(10)),
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 440),
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
              child: Row(children: [
                const Spacer(),
                TextButton(onPressed: () => Navigator.of(context).pop(false), child: const Text('Cancel')),
                const SizedBox(width: 8),
                FilledButton(
                  onPressed: busy ? null : onPrimary,
                  child: busy
                      ? const SizedBox(width: 15, height: 15, child: CircularProgressIndicator(strokeWidth: 2))
                      : Text(primaryLabel),
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
    for (final c in [_name, _apiBase, _orgId, _token, _username, _password]) {
      c.dispose();
    }
    super.dispose();
  }

  Future<void> _submit() async {
    final org = BigInt.tryParse(_orgId.text.trim());
    if (_name.text.trim().isEmpty) return setState(() => _error = 'Name is required.');
    if (org == null) return setState(() => _error = 'Organization ID must be a number.');
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      await widget.state.addProjectEntry(AddConnectionInput(
        name: _name.text.trim(),
        apiBase: _apiBase.text.trim(),
        orgId: org,
        authKind: _auth,
        token: _auth == AuthKind.token ? _token.text : null,
        username: _auth == AuthKind.password ? _username.text : null,
        password: _auth == AuthKind.password ? _password.text : null,
      ));
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
        _Field(label: 'NAME', controller: _name, autofocus: true),
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
    for (final c in [_name, _apiBase, _orgId, _token, _username, _password]) {
      c.dispose();
    }
    super.dispose();
  }

  Future<void> _submit() async {
    final org = BigInt.tryParse(_orgId.text.trim());
    if (_name.text.trim().isEmpty) return setState(() => _error = 'Name is required.');
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
        _Field(label: 'NAME', controller: _name, autofocus: true),
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
