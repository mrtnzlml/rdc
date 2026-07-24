import 'package:flutter/material.dart';

import 'app_state.dart';
import 'rust/api/rdc.dart';

/// Shared credential form used by both Add and Edit. Collects a token or a
/// username/password pair depending on the selected [AuthKind].
class _CredentialFields extends StatelessWidget {
  const _CredentialFields({
    required this.auth,
    required this.onAuthChanged,
    required this.token,
    required this.username,
    required this.password,
  });

  final AuthKind auth;
  final ValueChanged<AuthKind> onAuthChanged;
  final TextEditingController token;
  final TextEditingController username;
  final TextEditingController password;

  @override
  Widget build(BuildContext context) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        SegmentedButton<AuthKind>(
          segments: const [
            ButtonSegment(value: AuthKind.token, label: Text('API token')),
            ButtonSegment(value: AuthKind.password, label: Text('Username & password')),
          ],
          selected: {auth},
          onSelectionChanged: (s) => onAuthChanged(s.first),
        ),
        const SizedBox(height: 12),
        if (auth == AuthKind.token)
          TextField(
            controller: token,
            obscureText: true,
            decoration: const InputDecoration(labelText: 'API token'),
          )
        else ...[
          TextField(
            controller: username,
            decoration: const InputDecoration(labelText: 'Username'),
          ),
          const SizedBox(height: 8),
          TextField(
            controller: password,
            obscureText: true,
            decoration: const InputDecoration(labelText: 'Password'),
          ),
        ],
      ],
    );
  }
}

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
    if (_name.text.trim().isEmpty) {
      setState(() => _error = 'Name is required.');
      return;
    }
    if (org == null) {
      setState(() => _error = 'Organization ID must be a number.');
      return;
    }
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
        _error = '$e';
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('New connection'),
      content: SizedBox(
        width: 440,
        child: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              TextField(
                controller: _name,
                autofocus: true,
                decoration: const InputDecoration(labelText: 'Name'),
              ),
              const SizedBox(height: 8),
              TextField(
                controller: _apiBase,
                decoration: const InputDecoration(labelText: 'API base URL'),
              ),
              const SizedBox(height: 8),
              TextField(
                controller: _orgId,
                keyboardType: TextInputType.number,
                decoration: const InputDecoration(labelText: 'Organization ID'),
              ),
              const SizedBox(height: 16),
              _CredentialFields(
                auth: _auth,
                onAuthChanged: (a) => setState(() => _auth = a),
                token: _token,
                username: _username,
                password: _password,
              ),
              if (_error != null) ...[
                const SizedBox(height: 12),
                SelectableText(_error!,
                    style: TextStyle(color: Theme.of(context).colorScheme.error)),
              ],
            ],
          ),
        ),
      ),
      actions: [
        TextButton(
          onPressed: _busy ? null : () => Navigator.of(context).pop(false),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: _busy ? null : _submit,
          child: _busy
              ? const SizedBox(
                  width: 16, height: 16, child: CircularProgressIndicator(strokeWidth: 2))
              : const Text('Create'),
        ),
      ],
    );
  }
}

class EditCredentialsDialog extends StatefulWidget {
  const EditCredentialsDialog({super.key, required this.state, required this.item});
  final AppState state;
  final ConnItem item;

  @override
  State<EditCredentialsDialog> createState() => _EditCredentialsDialogState();
}

class _EditCredentialsDialogState extends State<EditCredentialsDialog> {
  final _token = TextEditingController();
  final _username = TextEditingController();
  final _password = TextEditingController();
  late AuthKind _auth = widget.item.summary.authKind;
  String? _error;
  bool _busy = false;

  @override
  void dispose() {
    for (final c in [_token, _username, _password]) {
      c.dispose();
    }
    super.dispose();
  }

  Future<void> _submit() async {
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      await widget.state.editCredentialsEntry(
        widget.item.summary.folder,
        EditCredentialsInput(
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
        _error = '$e';
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: Text('Edit credentials — ${widget.item.summary.name}'),
      content: SizedBox(
        width: 440,
        child: _CredentialFields(
          auth: _auth,
          onAuthChanged: (a) => setState(() => _auth = a),
          token: _token,
          username: _username,
          password: _password,
        ),
      ),
      actions: [
        if (_error != null)
          Padding(
            padding: const EdgeInsets.only(right: 8),
            child: SelectableText(_error!,
                style: TextStyle(color: Theme.of(context).colorScheme.error)),
          ),
        TextButton(
          onPressed: _busy ? null : () => Navigator.of(context).pop(false),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: _busy ? null : _submit,
          child: _busy
              ? const SizedBox(
                  width: 16, height: 16, child: CircularProgressIndicator(strokeWidth: 2))
              : const Text('Save'),
        ),
      ],
    );
  }
}
