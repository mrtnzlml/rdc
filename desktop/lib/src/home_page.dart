import 'dart:io';

import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';

import 'app_state.dart';
import 'dialogs.dart';
import 'rust/api/rdc.dart';
import 'update_check.dart';

class HomePage extends StatefulWidget {
  const HomePage({super.key, required this.state});
  final AppState state;

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
  AppState get state => widget.state;

  @override
  void initState() {
    super.initState();
    if (state.parentFolder != null) state.reload();
    _checkUpdate();
  }

  Future<void> _checkUpdate() async {
    if (Platform.environment.containsKey('FLUTTER_TEST')) return;
    final info = await checkForUpdate();
    if (info != null && mounted) {
      ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(
            'A newer version (${info.latest}) is available on GitHub Releases.'),
        duration: const Duration(seconds: 8),
      ));
    }
  }

  Future<void> _run(Future<void> Function() action) async {
    try {
      await action();
    } catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(content: SelectableText('$e')),
        );
      }
    }
  }

  Future<void> _chooseParent() async {
    final path = await getDirectoryPath(confirmButtonText: 'Choose');
    if (path != null) await _run(() => state.setParentFolder(path));
  }

  Future<void> _openExisting() async {
    final path = await getDirectoryPath(confirmButtonText: 'Open');
    if (path != null) await _run(() => state.openExisting(path));
  }

  Future<void> _addConnection() async {
    await showDialog<bool>(
      context: context,
      builder: (_) => AddConnectionDialog(state: state),
    );
  }

  Future<void> _editCredentials(ConnItem item) async {
    await showDialog<bool>(
      context: context,
      builder: (_) => EditCredentialsDialog(state: state, item: item),
    );
  }

  Future<void> _confirmRemove(ConnItem item) async {
    final external = item.isExternal;
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(external ? 'Detach connection?' : 'Remove connection?'),
        content: Text(external
            ? 'This forgets "${item.summary.name}" from the app. The folder on disk is left untouched.'
            : 'This moves "${item.summary.name}" and its files to the Trash.'),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx, false),
              child: const Text('Cancel')),
          FilledButton(
            onPressed: () => Navigator.pop(ctx, true),
            child: Text(external ? 'Detach' : 'Move to Trash'),
          ),
        ],
      ),
    );
    if (ok == true) await _run(() => state.removeOrDetach(item));
  }

  void _about() {
    showAboutDialog(
      context: context,
      applicationName: 'Rossum Local',
      applicationVersion: 'v$kAppVersion  •  rdc core embedded',
      children: const [
        Text('Cross-platform desktop front-end for the rdc core '
            '(Flutter + flutter_rust_bridge).'),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: state,
      builder: (context, _) {
        final hasParent = state.parentFolder != null;
        return Scaffold(
          appBar: AppBar(
            title: const Text('Rossum Local'),
            actions: [
              if (hasParent)
                IconButton(
                  tooltip: 'New connection',
                  icon: const Icon(Icons.add),
                  onPressed: _addConnection,
                ),
              if (hasParent)
                IconButton(
                  tooltip: 'Open existing rdc project',
                  icon: const Icon(Icons.folder_open),
                  onPressed: _openExisting,
                ),
              PopupMenuButton<String>(
                onSelected: (v) {
                  if (v == 'folder') _chooseParent();
                  if (v == 'about') _about();
                },
                itemBuilder: (_) => const [
                  PopupMenuItem(value: 'folder', child: Text('Change parent folder…')),
                  PopupMenuItem(value: 'about', child: Text('About Rossum Local')),
                ],
              ),
            ],
          ),
          body: !hasParent
              ? _EmptyState(onChoose: _chooseParent)
              : Row(
                  children: [
                    SizedBox(
                      width: 300,
                      child: _Sidebar(state: state),
                    ),
                    const VerticalDivider(width: 1),
                    Expanded(
                      child: _Detail(
                        state: state,
                        onSync: (i) => state.sync(i),
                        onEdit: _editCredentials,
                        onReveal: (i) => _run(() => state.reveal(i.summary.folder)),
                        onRemove: _confirmRemove,
                      ),
                    ),
                  ],
                ),
        );
      },
    );
  }
}

class _EmptyState extends StatelessWidget {
  const _EmptyState({required this.onChoose});
  final VoidCallback onChoose;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Icon(Icons.folder_outlined,
              size: 64, color: Theme.of(context).colorScheme.primary),
          const SizedBox(height: 16),
          Text('Choose a folder for your connections',
              style: Theme.of(context).textTheme.titleMedium),
          const SizedBox(height: 8),
          const SizedBox(
            width: 380,
            child: Text(
              'Pick a folder to hold your rdc projects. Each connection is a '
              'subfolder the CLI and this app share.',
              textAlign: TextAlign.center,
            ),
          ),
          const SizedBox(height: 20),
          FilledButton.icon(
            onPressed: onChoose,
            icon: const Icon(Icons.folder_open),
            label: const Text('Choose folder…'),
          ),
        ],
      ),
    );
  }
}

class _Sidebar extends StatelessWidget {
  const _Sidebar({required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    if (state.loading && state.connections.isEmpty) {
      return const Center(child: CircularProgressIndicator());
    }
    if (state.connections.isEmpty) {
      return const Center(
        child: Padding(
          padding: EdgeInsets.all(24),
          child: Text('No connections yet.\nUse + to add one.',
              textAlign: TextAlign.center),
        ),
      );
    }
    return ListView.builder(
      itemCount: state.connections.length,
      itemBuilder: (context, i) {
        final c = state.connections[i];
        final selected = c.summary.folder == state.selectedFolder;
        return ListTile(
          selected: selected,
          leading: _statusIcon(context, state.syncState[c.summary.folder]),
          title: Text(c.summary.name, overflow: TextOverflow.ellipsis),
          subtitle: Text(
            '${c.isExternal ? "external • " : ""}${c.summary.fileCount} files',
            overflow: TextOverflow.ellipsis,
          ),
          onTap: () => state.select(c.summary.folder),
        );
      },
    );
  }

  Widget _statusIcon(BuildContext context, SyncState? s) {
    switch (s) {
      case SyncState.running:
        return const SizedBox(
            width: 20, height: 20, child: CircularProgressIndicator(strokeWidth: 2));
      case SyncState.error:
        return Icon(Icons.error_outline, color: Theme.of(context).colorScheme.error);
      case SyncState.done:
        return const Icon(Icons.cloud_done_outlined, color: Colors.green);
      default:
        return const Icon(Icons.cloud_outlined);
    }
  }
}

class _Detail extends StatelessWidget {
  const _Detail({
    required this.state,
    required this.onSync,
    required this.onEdit,
    required this.onReveal,
    required this.onRemove,
  });

  final AppState state;
  final void Function(ConnItem) onSync;
  final void Function(ConnItem) onEdit;
  final void Function(ConnItem) onReveal;
  final void Function(ConnItem) onRemove;

  @override
  Widget build(BuildContext context) {
    final item = state.selected;
    if (item == null) {
      return const Center(child: Text('Select a connection'));
    }
    final s = item.summary;
    final syncState = state.syncState[s.folder];
    final syncMsg = state.syncMessage[s.folder];

    return SingleChildScrollView(
      padding: const EdgeInsets.all(24),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(s.name, style: Theme.of(context).textTheme.headlineSmall),
              ),
              if (item.isExternal)
                const Chip(label: Text('external')),
            ],
          ),
          const SizedBox(height: 16),
          _field('API base', s.apiBase),
          _field('Organization ID', s.orgId.toString()),
          _field('Authentication',
              s.authKind == AuthKind.token ? 'API token' : 'Username & password'),
          _field('Folder', s.folder),
          _field('Files', s.fileCount.toString()),
          _field('Last sync', _lastSync(s)),
          const SizedBox(height: 20),
          Wrap(
            spacing: 12,
            runSpacing: 12,
            children: [
              FilledButton.icon(
                onPressed:
                    syncState == SyncState.running ? null : () => onSync(item),
                icon: const Icon(Icons.sync),
                label: const Text('Sync'),
              ),
              OutlinedButton.icon(
                onPressed: () => onEdit(item),
                icon: const Icon(Icons.key),
                label: const Text('Edit credentials'),
              ),
              OutlinedButton.icon(
                onPressed: () => onReveal(item),
                icon: const Icon(Icons.open_in_new),
                label: const Text('Reveal'),
              ),
              OutlinedButton.icon(
                onPressed: () => onRemove(item),
                icon: Icon(item.isExternal ? Icons.link_off : Icons.delete_outline),
                label: Text(item.isExternal ? 'Detach' : 'Remove'),
              ),
            ],
          ),
          if (syncMsg != null) ...[
            const SizedBox(height: 20),
            Container(
              width: double.infinity,
              padding: const EdgeInsets.all(12),
              decoration: BoxDecoration(
                color: syncState == SyncState.error
                    ? Theme.of(context).colorScheme.errorContainer
                    : Theme.of(context).colorScheme.surfaceContainerHighest,
                borderRadius: BorderRadius.circular(8),
              ),
              child: SelectableText(
                syncMsg,
                style: TextStyle(
                  color: syncState == SyncState.error
                      ? Theme.of(context).colorScheme.onErrorContainer
                      : null,
                ),
              ),
            ),
          ],
        ],
      ),
    );
  }

  Widget _field(String label, String value) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 12),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(label,
              style: const TextStyle(fontSize: 12, fontWeight: FontWeight.w600)),
          const SizedBox(height: 2),
          SelectableText(value),
        ],
      ),
    );
  }

  String _lastSync(ConnectionSummary s) {
    final v = s.lastSyncUnix;
    if (v == null) return 'Never synced';
    final dt = DateTime.fromMillisecondsSinceEpoch(v.toInt() * 1000).toLocal();
    return dt.toString();
  }
}
