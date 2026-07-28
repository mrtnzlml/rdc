import 'package:flutter/material.dart';

import 'app_state.dart';
import 'console_theme.dart';
import 'error_text.dart';
import 'rust/api/rdc.dart';

// ------------------------------------------------------------ shared shell

/// Console-styled modal frame: an accent title bar, a body, and a footer with
/// a keyboard hint on the left and a primary button on the right.
class _Frame extends StatelessWidget {
  const _Frame({
    required this.title,
    required this.child,
    required this.footerHint,
    required this.primaryLabel,
    required this.onPrimary,
    this.busy = false,
  });

  final String title;
  final Widget child;
  final String footerHint;
  final String primaryLabel;
  final VoidCallback? onPrimary;
  final bool busy;

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    return Dialog(
      backgroundColor: c.surf,
      surfaceTintColor: Colors.transparent,
      shape: RoundedRectangleBorder(
        side: BorderSide(color: c.line),
        borderRadius: BorderRadius.circular(8),
      ),
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 460),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            // title bar
            Container(
              width: double.infinity,
              padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 9),
              decoration: BoxDecoration(
                color: c.acc.withValues(alpha: 0.10),
                border: Border(bottom: BorderSide(color: c.line)),
              ),
              child: Text('rdc ▸ $title',
                  style: TextStyle(color: c.acc, fontSize: 12, fontWeight: FontWeight.w600)),
            ),
            Flexible(
              child: SingleChildScrollView(
                padding: const EdgeInsets.all(14),
                child: child,
              ),
            ),
            // footer
            Container(
              width: double.infinity,
              padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 11),
              decoration: BoxDecoration(border: Border(top: BorderSide(color: c.line))),
              child: Row(
                children: [
                  Text(footerHint, style: TextStyle(color: c.muted, fontSize: 11)),
                  const Spacer(),
                  _PrimaryBtn(label: primaryLabel, busy: busy, onTap: onPrimary),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _PrimaryBtn extends StatelessWidget {
  const _PrimaryBtn({required this.label, required this.onTap, this.busy = false});
  final String label;
  final VoidCallback? onTap;
  final bool busy;
  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    return InkWell(
      onTap: busy ? null : onTap,
      borderRadius: BorderRadius.circular(6),
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
        decoration: BoxDecoration(
          color: c.acc.withValues(alpha: 0.10),
          border: Border.all(color: c.acc),
          borderRadius: BorderRadius.circular(6),
        ),
        child: busy
            ? SizedBox(width: 14, height: 14, child: CircularProgressIndicator(strokeWidth: 2, color: c.acc))
            : Text(label, style: TextStyle(color: c.acc, fontSize: 12, fontWeight: FontWeight.w600)),
      ),
    );
  }
}

/// A labelled console input: `key` on the left, a bordered field on the right.
class _Field extends StatelessWidget {
  const _Field({
    required this.label,
    required this.controller,
    this.obscure = false,
    this.highlight = false,
    this.placeholder,
    this.keyboardType,
    this.autofocus = false,
  });
  final String label;
  final TextEditingController controller;
  final bool obscure;
  final bool highlight;
  final String? placeholder;
  final TextInputType? keyboardType;
  final bool autofocus;

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    return Padding(
      padding: const EdgeInsets.only(bottom: 11),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.center,
        children: [
          SizedBox(width: 78, child: Text(label, style: TextStyle(color: c.muted, fontSize: 12.5))),
          const SizedBox(width: 12),
          Expanded(
            child: Container(
              decoration: BoxDecoration(
                color: c.log,
                border: Border.all(color: highlight ? c.acc : c.line),
                borderRadius: BorderRadius.circular(5),
              ),
              padding: const EdgeInsets.symmetric(horizontal: 9, vertical: 2),
              child: TextField(
                controller: controller,
                autofocus: autofocus,
                obscureText: obscure,
                keyboardType: keyboardType,
                style: TextStyle(color: c.ink, fontSize: 12.5),
                cursorColor: c.acc,
                decoration: InputDecoration(
                  isDense: true,
                  border: InputBorder.none,
                  hintText: placeholder,
                  hintStyle: TextStyle(color: c.muted, fontSize: 12.5),
                ),
              ),
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
    final c = ConsoleColors.of(context);
    Widget opt(AuthKind k, String label) {
      final sel = k == value;
      return Expanded(
        child: InkWell(
          onTap: () => onChanged(k),
          child: Container(
            alignment: Alignment.center,
            padding: const EdgeInsets.symmetric(vertical: 7),
            decoration: BoxDecoration(
              color: sel ? c.acc.withValues(alpha: 0.10) : Colors.transparent,
              border: Border.all(color: sel ? c.acc : c.line),
              borderRadius: BorderRadius.circular(5),
            ),
            child: Text(label, style: TextStyle(color: sel ? c.acc : c.muted, fontSize: 12)),
          ),
        ),
      );
    }

    return Padding(
      padding: const EdgeInsets.only(bottom: 11),
      child: Row(
        children: [
          SizedBox(width: 78, child: Text('auth', style: TextStyle(color: c.muted, fontSize: 12.5))),
          const SizedBox(width: 12),
          opt(AuthKind.token, 'api_token'),
          const SizedBox(width: 6),
          opt(AuthKind.password, 'user + pass'),
        ],
      ),
    );
  }
}

class _ErrLine extends StatelessWidget {
  const _ErrLine(this.text);
  final String text;
  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    return Padding(
      padding: const EdgeInsets.only(top: 4),
      child: SelectableText('✕ $text', style: TextStyle(color: c.err, fontSize: 12)),
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
      await widget.state.addConnectionEntry(AddConnectionInput(
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
      title: 'new connection',
      footerHint: '⏎ create · esc cancel',
      primaryLabel: '⏎ create',
      busy: _busy,
      onPrimary: _submit,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _Field(label: 'name', controller: _name, autofocus: true),
          _Field(label: 'api_base', controller: _apiBase),
          _Field(label: 'org_id', controller: _orgId, keyboardType: TextInputType.number),
          _AuthToggle(value: _auth, onChanged: (a) => setState(() => _auth = a)),
          if (_auth == AuthKind.token)
            _Field(label: 'token', controller: _token, obscure: true)
          else ...[
            _Field(label: 'username', controller: _username),
            _Field(label: 'password', controller: _password, obscure: true),
          ],
          if (_error != null) _ErrLine(_error!),
        ],
      ),
    );
  }
}

// ------------------------------------------------------------ edit

class EditConnectionDialog extends StatefulWidget {
  const EditConnectionDialog({super.key, required this.state, required this.item});
  final AppState state;
  final ConnItem item;
  @override
  State<EditConnectionDialog> createState() => _EditConnectionDialogState();
}

class _EditConnectionDialogState extends State<EditConnectionDialog> {
  late final _name = TextEditingController(text: widget.item.summary.name);
  late final _apiBase = TextEditingController(text: widget.item.summary.apiBase);
  late final _orgId = TextEditingController(text: widget.item.summary.orgId.toString());
  final _token = TextEditingController();
  final _username = TextEditingController();
  final _password = TextEditingController();
  late AuthKind _auth = widget.item.summary.authKind;
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
      await widget.state.editConnectionEntry(
        widget.item,
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
    final c = ConsoleColors.of(context);
    return _Frame(
      title: 'edit · ${widget.item.summary.name}',
      footerHint: '⏎ save · esc cancel',
      primaryLabel: '⏎ save',
      busy: _busy,
      onPrimary: _submit,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _Field(label: 'name', controller: _name, highlight: true, autofocus: true),
          _Field(label: 'api_base', controller: _apiBase),
          _Field(label: 'org_id', controller: _orgId, keyboardType: TextInputType.number),
          _AuthToggle(value: _auth, onChanged: (a) => setState(() => _auth = a)),
          if (_auth == AuthKind.token)
            _Field(label: 'token', controller: _token, obscure: true, placeholder: 'leave blank to keep current')
          else ...[
            _Field(label: 'username', controller: _username, placeholder: 'leave blank to keep current'),
            _Field(label: 'password', controller: _password, obscure: true, placeholder: 'leave blank to keep current'),
          ],
          Padding(
            padding: const EdgeInsets.only(top: 2),
            child: Text('leave credentials blank to keep the current ones',
                style: TextStyle(color: c.muted, fontSize: 11)),
          ),
          if (_error != null) _ErrLine(_error!),
        ],
      ),
    );
  }
}

// ------------------------------------------------------------ remove / detach

class RemoveDialog extends StatelessWidget {
  const RemoveDialog({super.key, required this.item});
  final ConnItem item;
  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    final external = item.isExternal;
    return _Frame(
      title: external ? 'detach · ${item.summary.name}' : 'remove · ${item.summary.name}',
      footerHint: '⏎ confirm · esc cancel',
      primaryLabel: external ? '⏎ detach' : '⏎ move to trash',
      onPrimary: () => Navigator.of(context).pop(true),
      child: Text(
        external
            ? 'Forgets "${item.summary.name}" from the app. The folder on disk is left untouched.'
            : 'Moves "${item.summary.name}" and its files to the Trash.',
        style: TextStyle(color: c.ink, fontSize: 13, height: 1.5),
      ),
    );
  }
}

// ------------------------------------------------------------ command palette

class PaletteCmd {
  PaletteCmd(this.label, {this.key, required this.run});
  final String label;
  final String? key;
  final VoidCallback run;
}

class CommandPalette extends StatefulWidget {
  const CommandPalette({super.key, required this.commands});
  final List<PaletteCmd> commands;
  @override
  State<CommandPalette> createState() => _CommandPaletteState();
}

class _CommandPaletteState extends State<CommandPalette> {
  final _q = TextEditingController();
  String _filter = '';

  List<PaletteCmd> get _matches => widget.commands
      .where((c) => c.label.toLowerCase().contains(_filter.toLowerCase()))
      .toList();

  void _choose(PaletteCmd cmd) => Navigator.of(context).pop(cmd);

  @override
  void dispose() {
    _q.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    final matches = _matches;
    return Dialog(
      alignment: Alignment.topCenter,
      insetPadding: const EdgeInsets.symmetric(horizontal: 40, vertical: 80),
      backgroundColor: c.surf,
      surfaceTintColor: Colors.transparent,
      shape: RoundedRectangleBorder(
        side: BorderSide(color: c.line),
        borderRadius: BorderRadius.circular(8),
      ),
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 520),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            // prompt row
            Container(
              padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
              decoration: BoxDecoration(border: Border(bottom: BorderSide(color: c.line))),
              child: Row(
                children: [
                  Text('rdc ▸', style: TextStyle(color: c.acc, fontWeight: FontWeight.w700, fontSize: 13)),
                  const SizedBox(width: 10),
                  Expanded(
                    child: TextField(
                      controller: _q,
                      autofocus: true,
                      style: TextStyle(color: c.ink, fontSize: 13),
                      cursorColor: c.acc,
                      decoration: InputDecoration(
                        isDense: true,
                        border: InputBorder.none,
                        hintText: 'type a command…',
                        hintStyle: TextStyle(color: c.muted, fontSize: 13),
                      ),
                      onChanged: (v) => setState(() => _filter = v),
                      onSubmitted: (_) {
                        if (matches.isNotEmpty) _choose(matches.first);
                      },
                    ),
                  ),
                ],
              ),
            ),
            Flexible(
              child: ListView.builder(
                shrinkWrap: true,
                padding: const EdgeInsets.symmetric(vertical: 4),
                itemCount: matches.length,
                itemBuilder: (context, i) {
                  final cmd = matches[i];
                  final first = i == 0;
                  return InkWell(
                    onTap: () => _choose(cmd),
                    child: Container(
                      color: first ? c.sel : Colors.transparent,
                      padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 9),
                      child: Row(
                        children: [
                          if (first) Text('▸ ', style: TextStyle(color: c.acc, fontSize: 13)),
                          Expanded(
                            child: Text(cmd.label,
                                style: TextStyle(color: c.ink, fontSize: 13), overflow: TextOverflow.ellipsis),
                          ),
                          if (cmd.key != null)
                            Container(
                              padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
                              decoration: BoxDecoration(
                                border: Border.all(color: c.line),
                                borderRadius: BorderRadius.circular(4),
                              ),
                              child: Text(cmd.key!, style: TextStyle(color: c.acc, fontSize: 11)),
                            ),
                        ],
                      ),
                    ),
                  );
                },
              ),
            ),
            Container(
              width: double.infinity,
              padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
              decoration: BoxDecoration(border: Border(top: BorderSide(color: c.line))),
              child: Text('⏎ run · esc close', style: TextStyle(color: c.muted, fontSize: 11)),
            ),
          ],
        ),
      ),
    );
  }
}
