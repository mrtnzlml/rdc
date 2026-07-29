import 'dart:io';

import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';

import 'ansi.dart';
import 'app_state.dart';
import 'dialogs.dart';
import 'error_text.dart';
import 'mdh_theme.dart';
import 'rust/api/rdc.dart';
import 'update_check.dart';

// ------------------------------------------------------------ helpers

enum RailView { connections, overview, settings }

enum _St { running, error, synced, never }

_St _statusOf(AppState s, ConnItem it) {
  switch (s.syncState[it.summary.folder]) {
    case SyncState.running:
      return _St.running;
    case SyncState.error:
      return _St.error;
    default:
      return it.summary.lastSyncUnix != null ? _St.synced : _St.never;
  }
}

String _rel(int? unix) {
  if (unix == null) return 'never';
  final d = DateTime.now()
      .difference(DateTime.fromMillisecondsSinceEpoch(unix * 1000));
  if (d.inSeconds < 60) return 'now';
  if (d.inMinutes < 60) return '${d.inMinutes}m';
  if (d.inHours < 24) return '${d.inHours}h';
  return '${d.inDays}d';
}

String _host(String apiBase) {
  var s = apiBase.replaceFirst(RegExp(r'^https?://'), '');
  final slash = s.indexOf('/');
  return slash >= 0 ? s.substring(0, slash) : s;
}

TextStyle _mono(Color color, double size, [FontWeight w = FontWeight.w400]) =>
    TextStyle(color: color, fontSize: size, fontWeight: w,
        fontFamily: kMonoFamily, fontFamilyFallback: kMonoFallback);

// ------------------------------------------------------------ page

class HomePage extends StatefulWidget {
  const HomePage({super.key, required this.state});
  final AppState state;
  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
  AppState get state => widget.state;
  RailView _rail = RailView.connections;
  double _listWidth = 250;
  String _tab = 'overview';

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
        content: Text('A newer version (${info.latest}) is available on GitHub Releases.'),
        duration: const Duration(seconds: 8),
      ));
    }
  }

  Future<void> _run(Future<void> Function() action) async {
    try {
      await action();
    } catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context)
            .showSnackBar(SnackBar(content: SelectableText(errorText(e))));
      }
    }
  }

  Future<void> _chooseParent() => _run(() async {
        final p = await getDirectoryPath(confirmButtonText: 'Choose');
        if (p != null) await state.setParentFolder(p);
      });

  Future<void> _openExisting() => _run(() async {
        final p = await getDirectoryPath(confirmButtonText: 'Open');
        if (p != null) await state.openExisting(p);
      });

  Future<void> _addConnection() =>
      showDialog<bool>(context: context, builder: (_) => AddConnectionDialog(state: state));

  Future<void> _editConnection(ConnItem i) =>
      showDialog<bool>(context: context, builder: (_) => EditConnectionDialog(state: state, item: i));

  Future<void> _reveal(ConnItem i) => _run(() => state.reveal(i.summary.folder));

  Future<void> _confirmRemove(ConnItem i) async {
    final ok = await showDialog<bool>(context: context, builder: (_) => RemoveDialog(item: i));
    if (ok == true) await _run(() => state.removeOrDetach(i));
  }

  void _syncAll() {
    for (final c in state.connections) {
      state.sync(c);
    }
  }

  void _about() => showAboutDialog(
        context: context,
        applicationName: 'rdc',
        applicationVersion: 'v$kAppVersion  •  rdc core embedded',
        children: const [
          Text('Cross-platform desktop front-end for the rdc core '
              '(Flutter + flutter_rust_bridge).'),
        ],
      );

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: ListenableBuilder(
        listenable: state,
        builder: (context, _) => MdhScaffold(
          state: state,
          view: _rail,
          listWidth: _listWidth,
          activeTab: _tab,
          onSelectRail: (v) => setState(() => _rail = v),
          onResize: (dx) => setState(() => _listWidth = (_listWidth + dx).clamp(200.0, 460.0)),
          onSelectTab: (t) => setState(() => _tab = t),
          onSelectConn: (folder) => setState(() {
            state.select(folder);
            _rail = RailView.connections;
          }),
          onAdd: _addConnection,
          onOpen: _openExisting,
          onSync: (i) => state.sync(i),
          onSyncAll: _syncAll,
          onEdit: _editConnection,
          onReveal: _reveal,
          onRemove: _confirmRemove,
          onChooseParent: _chooseParent,
          onAbout: _about,
          onCheckUpdate: () async {
            final messenger = ScaffoldMessenger.of(context);
            final info = await checkForUpdate();
            if (!mounted) return;
            messenger.showSnackBar(SnackBar(
              content: Text(info == null
                  ? "You're on the latest version (or the check couldn't reach GitHub)."
                  : 'A newer version (${info.latest}) is available on GitHub Releases.'),
            ));
          },
        ),
      ),
    );
  }
}

// ------------------------------------------------------------ shell

/// The MDH-style shell: a rail + the active view. Public and callback-driven so
/// it renders in golden tests from seeded state, with no bridge dependence.
class MdhScaffold extends StatelessWidget {
  const MdhScaffold({
    super.key,
    required this.state,
    this.view = RailView.connections,
    this.listWidth = 250,
    this.activeTab = 'overview',
    this.onSelectRail,
    this.onResize,
    this.onSelectTab,
    this.onSelectConn,
    this.onAdd,
    this.onOpen,
    this.onSync,
    this.onSyncAll,
    this.onEdit,
    this.onReveal,
    this.onRemove,
    this.onChooseParent,
    this.onAbout,
    this.onCheckUpdate,
  });

  final AppState state;
  final RailView view;
  final double listWidth;
  final String activeTab;
  final void Function(RailView)? onSelectRail;
  final void Function(double)? onResize;
  final void Function(String)? onSelectTab;
  final void Function(String folder)? onSelectConn;
  final VoidCallback? onAdd;
  final VoidCallback? onOpen;
  final void Function(ConnItem)? onSync;
  final VoidCallback? onSyncAll;
  final void Function(ConnItem)? onEdit;
  final void Function(ConnItem)? onReveal;
  final void Function(ConnItem)? onRemove;
  final VoidCallback? onChooseParent;
  final VoidCallback? onAbout;
  final VoidCallback? onCheckUpdate;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final noParent = state.parentFolder == null;

    Widget content;
    if (noParent) {
      content = _ChooseFolderEmpty(onChoose: onChooseParent ?? () {});
    } else {
      content = switch (view) {
        RailView.connections => _ConnectionsView(
            state: state,
            listWidth: listWidth,
            activeTab: activeTab,
            onResize: onResize ?? (_) {},
            onSelectTab: onSelectTab ?? (_) {},
            onSelect: (f) => state.select(f),
            onAdd: onAdd ?? () {},
            onOpen: onOpen ?? () {},
            onSync: onSync ?? (_) {},
            onEdit: onEdit ?? (_) {},
            onReveal: onReveal ?? (_) {},
            onRemove: onRemove ?? (_) {},
          ),
        RailView.overview => _FleetView(
            state: state,
            onNew: onAdd ?? () {},
            onSyncAll: onSyncAll ?? () {},
            onOpenConn: onSelectConn ?? (_) {},
          ),
        RailView.settings => _SettingsView(
            state: state,
            onChooseParent: onChooseParent ?? () {},
            onOpen: onOpen ?? () {},
            onAbout: onAbout ?? () {},
            onCheckUpdate: onCheckUpdate ?? () {},
          ),
      };
    }

    return Container(
      color: c.bgBase,
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          _Rail(active: view, onSelect: onSelectRail ?? (_) {}),
          Expanded(child: content),
        ],
      ),
    );
  }
}

// ------------------------------------------------------------ rail

class _Rail extends StatelessWidget {
  const _Rail({required this.active, required this.onSelect});
  final RailView active;
  final void Function(RailView) onSelect;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    Widget item(RailView v, IconData icon, String tip) {
      final on = v == active;
      return Padding(
        padding: const EdgeInsets.symmetric(vertical: 3),
        child: Tooltip(
          message: tip,
          child: InkWell(
            onTap: () => onSelect(v),
            borderRadius: BorderRadius.circular(8),
            child: Container(
              width: 34,
              height: 34,
              decoration: BoxDecoration(
                color: on ? c.accent : Colors.transparent,
                borderRadius: BorderRadius.circular(8),
              ),
              child: Icon(icon, size: 18, color: on ? Colors.white : c.textSecondary),
            ),
          ),
        ),
      );
    }

    return Container(
      width: 52,
      decoration: BoxDecoration(
        color: c.bgSidebar,
        border: Border(right: BorderSide(color: c.border)),
      ),
      padding: const EdgeInsets.symmetric(vertical: 10),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.center,
        children: [
          Container(
            width: 32,
            height: 32,
            margin: const EdgeInsets.only(bottom: 8),
            decoration: BoxDecoration(color: c.accent, borderRadius: BorderRadius.circular(8)),
            alignment: Alignment.center,
            child: Text('rdc', style: _mono(Colors.white, 11, FontWeight.w700)),
          ),
          item(RailView.connections, Icons.dns_outlined, 'Connections'),
          item(RailView.overview, Icons.dashboard_outlined, 'Fleet overview'),
          const Spacer(),
          item(RailView.settings, Icons.settings_outlined, 'Settings'),
        ],
      ),
    );
  }
}

// ------------------------------------------------------------ connections view

class _ConnectionsView extends StatelessWidget {
  const _ConnectionsView({
    required this.state,
    required this.listWidth,
    required this.activeTab,
    required this.onResize,
    required this.onSelectTab,
    required this.onSelect,
    required this.onAdd,
    required this.onOpen,
    required this.onSync,
    required this.onEdit,
    required this.onReveal,
    required this.onRemove,
  });
  final AppState state;
  final double listWidth;
  final String activeTab;
  final void Function(double) onResize;
  final void Function(String) onSelectTab;
  final void Function(String) onSelect;
  final VoidCallback onAdd, onOpen;
  final void Function(ConnItem) onSync, onEdit, onReveal, onRemove;

  @override
  Widget build(BuildContext context) {
    return Row(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        SizedBox(
          width: listWidth,
          child: _Sidebar(state: state, onSelect: onSelect, onAdd: onAdd, onOpen: onOpen),
        ),
        _Resizer(onDelta: onResize),
        Expanded(
          child: _ConnMain(
            state: state,
            activeTab: activeTab,
            onSelectTab: onSelectTab,
            onSync: onSync,
            onEdit: onEdit,
            onReveal: onReveal,
            onRemove: onRemove,
            onAdd: onAdd,
          ),
        ),
      ],
    );
  }
}

class _Sidebar extends StatelessWidget {
  const _Sidebar({required this.state, required this.onSelect, required this.onAdd, required this.onOpen});
  final AppState state;
  final void Function(String) onSelect;
  final VoidCallback onAdd, onOpen;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Container(
      decoration: BoxDecoration(
        color: c.bgSidebar,
        border: Border(right: BorderSide(color: c.border)),
      ),
      child: Column(
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(14, 12, 10, 8),
            child: Row(
              children: [
                Text('CONNECTIONS',
                    style: TextStyle(color: c.textSecondary, fontSize: 11, fontWeight: FontWeight.w700, letterSpacing: 0.8)),
                const Spacer(),
                Tooltip(
                  message: 'New connection',
                  child: InkWell(
                    onTap: onAdd,
                    borderRadius: BorderRadius.circular(6),
                    child: Container(
                      width: 24,
                      height: 24,
                      decoration: BoxDecoration(
                        color: c.bgCard,
                        border: Border.all(color: c.border),
                        borderRadius: BorderRadius.circular(6),
                      ),
                      child: Icon(Icons.add, size: 15, color: c.textSecondary),
                    ),
                  ),
                ),
              ],
            ),
          ),
          Expanded(
            child: state.connections.isEmpty
                ? Center(
                    child: Padding(
                      padding: const EdgeInsets.all(16),
                      child: Text('No connections yet.\nAdd one with +.',
                          textAlign: TextAlign.center,
                          style: TextStyle(color: c.textSecondary, fontSize: 12)),
                    ),
                  )
                : ListView(
                    padding: const EdgeInsets.symmetric(horizontal: 8),
                    children: [
                      for (final it in state.connections) _SidebarRow(state: state, item: it, onSelect: onSelect),
                    ],
                  ),
          ),
          Container(
            decoration: BoxDecoration(border: Border(top: BorderSide(color: c.border))),
            padding: const EdgeInsets.all(8),
            child: _NavItem(icon: Icons.folder_open_outlined, label: 'Open existing…', onTap: onOpen),
          ),
        ],
      ),
    );
  }
}

class _SidebarRow extends StatelessWidget {
  const _SidebarRow({required this.state, required this.item, required this.onSelect});
  final AppState state;
  final ConnItem item;
  final void Function(String) onSelect;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final s = item.summary;
    final sel = s.folder == state.selectedFolder;
    final st = _statusOf(state, item);
    final dotColor = switch (st) {
      _St.error => c.danger,
      _St.never => c.textHint,
      _ => c.successFg,
    };
    final sub = switch (st) {
      _St.running => 'syncing…',
      _St.error => 'failed',
      _St.synced => _rel(s.lastSyncUnix),
      _St.never => 'never',
    };
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 1),
      child: InkWell(
        onTap: () => onSelect(s.folder),
        borderRadius: BorderRadius.circular(6),
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
          decoration: BoxDecoration(
            color: sel ? c.accent : Colors.transparent,
            borderRadius: BorderRadius.circular(6),
          ),
          child: Row(
            children: [
              Container(width: 7, height: 7, margin: const EdgeInsets.only(right: 10),
                  decoration: BoxDecoration(color: sel ? Colors.white : dotColor, shape: BoxShape.circle)),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(s.name,
                        overflow: TextOverflow.ellipsis,
                        style: TextStyle(color: sel ? Colors.white : c.textPrimary, fontSize: 13, fontWeight: FontWeight.w500)),
                    const SizedBox(height: 2),
                    Text('org ${s.orgId} · $sub${item.isExternal ? ' · ext' : ''}',
                        overflow: TextOverflow.ellipsis,
                        style: _mono(sel ? Colors.white70 : c.textSecondary, 11)),
                  ],
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _Resizer extends StatelessWidget {
  const _Resizer({required this.onDelta});
  final void Function(double) onDelta;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return MouseRegion(
      cursor: SystemMouseCursors.resizeLeftRight,
      child: GestureDetector(
        behavior: HitTestBehavior.translucent,
        onHorizontalDragUpdate: (d) => onDelta(d.delta.dx),
        child: SizedBox(width: 5, child: Center(child: Container(width: 1, color: c.border))),
      ),
    );
  }
}

class _ConnMain extends StatelessWidget {
  const _ConnMain({
    required this.state,
    required this.activeTab,
    required this.onSelectTab,
    required this.onSync,
    required this.onEdit,
    required this.onReveal,
    required this.onRemove,
    required this.onAdd,
  });
  final AppState state;
  final String activeTab;
  final void Function(String) onSelectTab;
  final void Function(ConnItem) onSync, onEdit, onReveal, onRemove;
  final VoidCallback onAdd;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final item = state.selected;
    if (item == null) {
      return Center(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text('No connection selected', style: TextStyle(color: c.textSecondary)),
            const SizedBox(height: 12),
            _Btn(label: 'New connection', primary: true, onTap: onAdd),
          ],
        ),
      );
    }
    final st = _statusOf(state, item);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _ConnBar(state: state, item: item, onSync: onSync, onEdit: onEdit, onReveal: onReveal, onRemove: onRemove),
        _TabBar(active: activeTab, tabs: const ['overview', 'log'], labels: const {'overview': 'Overview', 'log': 'Sync log'}, onSelect: onSelectTab),
        Expanded(
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(18),
            child: activeTab == 'log'
                ? _SyncLogCard(state: state, item: item, expanded: true)
                : _OverviewPanel(state: state, item: item, st: st),
          ),
        ),
      ],
    );
  }
}

class _ConnBar extends StatelessWidget {
  const _ConnBar({required this.state, required this.item, required this.onSync, required this.onEdit, required this.onReveal, required this.onRemove});
  final AppState state;
  final ConnItem item;
  final void Function(ConnItem) onSync, onEdit, onReveal, onRemove;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final s = item.summary;
    final st = _statusOf(state, item);
    return Container(
      padding: const EdgeInsets.fromLTRB(16, 12, 16, 12),
      decoration: BoxDecoration(
        color: c.bgCard,
        border: Border(bottom: BorderSide(color: c.border)),
      ),
      child: Row(
        children: [
          Flexible(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: MainAxisSize.min,
              children: [
                Text(s.name, maxLines: 1, overflow: TextOverflow.ellipsis,
                    style: TextStyle(color: c.textPrimary, fontSize: 15, fontWeight: FontWeight.w600)),
                const SizedBox(height: 2),
                Text('${_host(s.apiBase)} · org ${s.orgId}', maxLines: 1, overflow: TextOverflow.ellipsis,
                    style: _mono(c.textSecondary, 12)),
              ],
            ),
          ),
          const SizedBox(width: 12),
          _StatusPill(st: st),
          const Spacer(),
          _Btn(label: st == _St.error ? 'Retry' : 'Sync', primary: true, onTap: st == _St.running ? null : () => onSync(item)),
          const SizedBox(width: 8),
          _Btn(label: 'Edit', onTap: () => onEdit(item)),
          const SizedBox(width: 8),
          _Btn(label: 'Reveal', onTap: () => onReveal(item)),
          const SizedBox(width: 8),
          _Btn(label: item.isExternal ? 'Detach' : 'Remove', onTap: () => onRemove(item)),
        ],
      ),
    );
  }
}

class _OverviewPanel extends StatelessWidget {
  const _OverviewPanel({required this.state, required this.item, required this.st});
  final AppState state;
  final ConnItem item;
  final _St st;

  @override
  Widget build(BuildContext context) {
    final s = item.summary;
    final auth = s.authKind == AuthKind.token ? 'API token' : 'Username & password';
    final lastSync = switch (st) {
      _St.running => 'syncing…',
      _St.error => 'failed · ${_rel(s.lastSyncUnix)}',
      _St.synced => 'today · ${_rel(s.lastSyncUnix)} ago',
      _St.never => 'never',
    };
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(children: [
          Expanded(child: _StatCard(n: s.fileCount.toString(), l: 'Files pulled')),
          const SizedBox(width: 12),
          Expanded(child: _StatCard(n: st == _St.never ? '—' : _rel(s.lastSyncUnix), l: 'Last sync')),
          const SizedBox(width: 12),
          Expanded(child: _StatCard(n: s.authKind == AuthKind.token ? 'token' : 'login', l: 'Auth')),
        ]),
        _SectionTitle('Connection'),
        Wrap(spacing: 12, runSpacing: 12, children: [
          _SpecCard(k: 'API base', v: s.apiBase),
          _SpecCard(k: 'Organization ID', v: s.orgId.toString()),
          _SpecCard(k: 'Folder', v: s.folder),
          _SpecCard(k: 'Authentication', v: auth, mono: false),
          _SpecCard(k: 'Last sync', v: lastSync, mono: false),
        ]),
        _SectionTitle('Recent sync'),
        _SyncLogCard(state: state, item: item, expanded: false),
      ],
    );
  }
}

class _SyncLogCard extends StatelessWidget {
  const _SyncLogCard({required this.state, required this.item, required this.expanded});
  final AppState state;
  final ConnItem item;
  final bool expanded;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final st = _statusOf(state, item);
    final lines = state.syncLog[item.summary.folder] ?? const <String>[];
    final msg = state.syncMessage[item.summary.folder];

    Widget body;
    if (lines.isNotEmpty) {
      final spans = <InlineSpan>[];
      for (var k = 0; k < lines.length; k++) {
        spans.addAll(ansiSpans(lines[k], c, 12.5));
        if (k < lines.length - 1) spans.add(const TextSpan(text: '\n'));
      }
      body = SizedBox(
        width: double.infinity,
        child: ConstrainedBox(
          constraints: BoxConstraints(maxHeight: expanded ? 420 : 150),
          child: Scrollbar(
            child: SingleChildScrollView(
              reverse: true,
              child: SelectableText.rich(TextSpan(children: spans)),
            ),
          ),
        ),
      );
    } else {
      final (String text, Color col) = switch (st) {
        _St.running => ('syncing…', c.textPrimary),
        _St.error => ('✕ ${msg ?? 'sync failed'}', c.dangerFg),
        _St.synced => ('✓ ${msg ?? 'up to date · ${_rel(item.summary.lastSyncUnix)} ago'}', c.successFg),
        _St.never => ('— not synced yet', c.textSecondary),
      };
      body = SelectableText(text, style: _mono(col, 12.5));
    }

    return Container(
      width: double.infinity,
      decoration: BoxDecoration(
        color: c.bgCode,
        border: Border.all(color: c.borderCard),
        borderRadius: BorderRadius.circular(6),
      ),
      padding: const EdgeInsets.fromLTRB(14, 12, 14, 12),
      child: body,
    );
  }
}

// ------------------------------------------------------------ fleet overview

class _FleetView extends StatelessWidget {
  const _FleetView({required this.state, required this.onNew, required this.onSyncAll, required this.onOpenConn});
  final AppState state;
  final VoidCallback onNew, onSyncAll;
  final void Function(String) onOpenConn;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final conns = state.connections;
    final now = DateTime.now();
    var syncedToday = 0, files = 0, errors = 0;
    for (final it in conns) {
      final u = it.summary.lastSyncUnix;
      if (u != null && now.difference(DateTime.fromMillisecondsSinceEpoch(u * 1000)).inHours < 24) {
        syncedToday++;
      }
      files += it.summary.fileCount.toInt();
      if (state.syncState[it.summary.folder] == SyncState.error) errors++;
    }

    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Container(
          padding: const EdgeInsets.fromLTRB(16, 12, 16, 12),
          decoration: BoxDecoration(color: c.bgCard, border: Border(bottom: BorderSide(color: c.border))),
          child: Row(children: [
            Column(crossAxisAlignment: CrossAxisAlignment.start, mainAxisSize: MainAxisSize.min, children: [
              Text('Fleet overview', style: TextStyle(color: c.textPrimary, fontSize: 15, fontWeight: FontWeight.w600)),
              const SizedBox(height: 2),
              Text(state.parentFolder ?? '', style: _mono(c.textSecondary, 12)),
            ]),
            const Spacer(),
            _Btn(label: 'Sync all', primary: true, onTap: conns.isEmpty ? null : onSyncAll),
            const SizedBox(width: 8),
            _Btn(label: 'New connection', onTap: onNew),
          ]),
        ),
        Expanded(
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(18),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(children: [
                  Expanded(child: _StatCard(n: '${conns.length}', l: 'Connections')),
                  const SizedBox(width: 12),
                  Expanded(child: _StatCard(n: '$syncedToday', l: 'Synced today')),
                  const SizedBox(width: 12),
                  Expanded(child: _StatCard(n: '$errors', l: 'Needs attention', danger: errors > 0)),
                  const SizedBox(width: 12),
                  Expanded(child: _StatCard(n: '$files', l: 'Files pulled')),
                ]),
                _SectionTitle('Connections'),
                _FleetTable(state: state, onOpenConn: onOpenConn),
              ],
            ),
          ),
        ),
      ],
    );
  }
}

class _FleetTable extends StatelessWidget {
  const _FleetTable({required this.state, required this.onOpenConn});
  final AppState state;
  final void Function(String) onOpenConn;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    Widget cell(Widget child, {int flex = 1}) =>
        Expanded(flex: flex, child: Padding(padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 11), child: child));
    Widget head(String t, {int flex = 1}) => cell(
        Text(t.toUpperCase(), style: TextStyle(color: c.textSecondary, fontSize: 10.5, fontWeight: FontWeight.w600, letterSpacing: 0.5)),
        flex: flex);

    if (state.connections.isEmpty) {
      return Container(
        width: double.infinity,
        padding: const EdgeInsets.all(24),
        decoration: BoxDecoration(color: c.bgCard, border: Border.all(color: c.borderCard), borderRadius: BorderRadius.circular(6)),
        child: Center(child: Text('No connections yet.', style: TextStyle(color: c.textSecondary))),
      );
    }

    return Container(
      decoration: BoxDecoration(color: c.bgCard, border: Border.all(color: c.borderCard), borderRadius: BorderRadius.circular(6),
          boxShadow: [BoxShadow(color: Colors.black.withValues(alpha: 0.04), blurRadius: 3, offset: const Offset(0, 1))]),
      clipBehavior: Clip.antiAlias,
      child: Column(children: [
        Container(
          decoration: BoxDecoration(color: c.bgSidebar, border: Border(bottom: BorderSide(color: c.border))),
          child: Row(children: [head('Connection', flex: 2), head('Org'), head('Host', flex: 2), head('Files'), head('Last sync'), head('Status')]),
        ),
        for (var i = 0; i < state.connections.length; i++)
          _FleetRow(state: state, item: state.connections[i], last: i == state.connections.length - 1, onOpenConn: onOpenConn, cell: cell),
      ]),
    );
  }
}

class _FleetRow extends StatelessWidget {
  const _FleetRow({required this.state, required this.item, required this.last, required this.onOpenConn, required this.cell});
  final AppState state;
  final ConnItem item;
  final bool last;
  final void Function(String) onOpenConn;
  final Widget Function(Widget, {int flex}) cell;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final s = item.summary;
    final st = _statusOf(state, item);
    final (String badge, Color bg, Color fg) = switch (st) {
      _St.error => ('error', c.dangerBg, c.dangerFg),
      _St.never => ('never', c.infoBg, c.infoFg),
      _St.running => ('syncing', c.infoBg, c.infoFg),
      _St.synced => ('synced', c.successBg, c.successFg),
    };
    return InkWell(
      onTap: () => onOpenConn(s.folder),
      child: Container(
        decoration: BoxDecoration(border: last ? null : Border(bottom: BorderSide(color: c.border))),
        child: Row(children: [
          cell(Row(children: [
            Flexible(child: Text(s.name, overflow: TextOverflow.ellipsis, style: TextStyle(color: c.textPrimary, fontSize: 12.5))),
            if (item.isExternal) Padding(padding: const EdgeInsets.only(left: 6), child: _MiniBadge('external', c.extBg, c.extFg)),
          ]), flex: 2),
          cell(Text(s.orgId.toString(), style: _mono(c.textSecondary, 12)), ),
          cell(Text(_host(s.apiBase), overflow: TextOverflow.ellipsis, style: _mono(c.textSecondary, 12)), flex: 2),
          cell(Text(s.fileCount.toString(), style: _mono(c.textSecondary, 12))),
          cell(Text(st == _St.never ? '—' : '${_rel(s.lastSyncUnix)} ago', style: TextStyle(color: c.textPrimary, fontSize: 12.5))),
          cell(Align(alignment: Alignment.centerLeft, child: _MiniBadge(badge, bg, fg))),
        ]),
      ),
    );
  }
}

// ------------------------------------------------------------ settings

class _SettingsView extends StatelessWidget {
  const _SettingsView({required this.state, required this.onChooseParent, required this.onOpen, required this.onAbout, required this.onCheckUpdate});
  final AppState state;
  final VoidCallback onChooseParent, onOpen, onAbout, onCheckUpdate;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Container(
          padding: const EdgeInsets.fromLTRB(16, 12, 16, 12),
          decoration: BoxDecoration(color: c.bgCard, border: Border(bottom: BorderSide(color: c.border))),
          child: Text('Settings', style: TextStyle(color: c.textPrimary, fontSize: 15, fontWeight: FontWeight.w600)),
        ),
        Expanded(
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(18),
            child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
              _SpecCard(k: 'Connections folder', v: state.parentFolder ?? '(not set)'),
              const SizedBox(height: 14),
              Wrap(spacing: 12, runSpacing: 12, children: [
                _Btn(label: 'Change folder…', onTap: onChooseParent),
                _Btn(label: 'Open existing project…', onTap: onOpen),
                _Btn(label: 'Check for updates', onTap: onCheckUpdate),
                _Btn(label: 'About rdc', onTap: onAbout),
              ]),
            ]),
          ),
        ),
      ],
    );
  }
}

// ------------------------------------------------------------ empty

class _ChooseFolderEmpty extends StatelessWidget {
  const _ChooseFolderEmpty({required this.onChoose});
  final VoidCallback onChoose;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Center(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 360),
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          Container(
            width: 56, height: 56, margin: const EdgeInsets.only(bottom: 14),
            decoration: BoxDecoration(color: c.infoBg, borderRadius: BorderRadius.circular(14)),
            child: Icon(Icons.folder_outlined, color: c.accent, size: 26),
          ),
          Text('Choose a folder for your connections', style: TextStyle(color: c.textPrimary, fontSize: 16, fontWeight: FontWeight.w600)),
          const SizedBox(height: 6),
          Text('Each connection is a subfolder that the CLI and this app share.',
              textAlign: TextAlign.center, style: TextStyle(color: c.textSecondary)),
          const SizedBox(height: 18),
          _Btn(label: 'Choose folder…', primary: true, onTap: onChoose),
        ]),
      ),
    );
  }
}

// ------------------------------------------------------------ shared bits

class _Btn extends StatelessWidget {
  const _Btn({required this.label, this.primary = false, this.onTap});
  final String label;
  final bool primary;
  final VoidCallback? onTap;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Opacity(
      opacity: onTap == null ? 0.5 : 1,
      child: InkWell(
        onTap: onTap,
        borderRadius: BorderRadius.circular(6),
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
          decoration: BoxDecoration(
            color: primary ? c.accent : c.bgCard,
            border: Border.all(color: primary ? c.accent : c.border),
            borderRadius: BorderRadius.circular(6),
          ),
          child: Text(label, style: TextStyle(color: primary ? Colors.white : c.textPrimary, fontSize: 12.5, fontWeight: FontWeight.w600)),
        ),
      ),
    );
  }
}

class _StatusPill extends StatelessWidget {
  const _StatusPill({required this.st});
  final _St st;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final (String label, Color bg, Color fg, Color bd) = switch (st) {
      _St.running => ('● syncing', c.infoBg, c.infoFg, c.infoBorder),
      _St.error => ('✕ sync failed', c.dangerBg, c.dangerFg, c.dangerBorder),
      _St.synced => ('● synced', c.successBg, c.successFg, c.successBorder),
      _St.never => ('○ never synced', c.warningBg, c.warningFg, c.warningBorder),
    };
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 9, vertical: 4),
      decoration: BoxDecoration(color: bg, border: Border.all(color: bd), borderRadius: BorderRadius.circular(999)),
      child: Text(label, style: TextStyle(color: fg, fontSize: 11, fontWeight: FontWeight.w600)),
    );
  }
}

class _TabBar extends StatelessWidget {
  const _TabBar({required this.active, required this.tabs, required this.labels, required this.onSelect});
  final String active;
  final List<String> tabs;
  final Map<String, String> labels;
  final void Function(String) onSelect;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Container(
      decoration: BoxDecoration(color: c.bgCard, border: Border(bottom: BorderSide(color: c.border))),
      padding: const EdgeInsets.symmetric(horizontal: 12),
      child: Row(children: [
        for (final t in tabs)
          InkWell(
            onTap: () => onSelect(t),
            child: Container(
              padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 11),
              decoration: BoxDecoration(
                border: Border(bottom: BorderSide(color: t == active ? c.accent : Colors.transparent, width: 2)),
              ),
              child: Text(labels[t] ?? t,
                  style: TextStyle(color: t == active ? c.accent : c.textSecondary, fontSize: 12.5, fontWeight: FontWeight.w600)),
            ),
          ),
      ]),
    );
  }
}

class _StatCard extends StatelessWidget {
  const _StatCard({required this.n, required this.l, this.danger = false});
  final String n, l;
  final bool danger;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Container(
      padding: const EdgeInsets.fromLTRB(15, 14, 15, 15),
      decoration: BoxDecoration(
        color: c.bgCard, border: Border.all(color: c.borderCard), borderRadius: BorderRadius.circular(6),
        boxShadow: [BoxShadow(color: Colors.black.withValues(alpha: 0.04), blurRadius: 3, offset: const Offset(0, 1))],
      ),
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Text(n, style: TextStyle(color: danger ? c.danger : c.textPrimary, fontSize: 26, fontWeight: FontWeight.w700, letterSpacing: -0.5)),
        const SizedBox(height: 7),
        Text(l.toUpperCase(), style: TextStyle(color: c.textSecondary, fontSize: 11, fontWeight: FontWeight.w600, letterSpacing: 0.5)),
      ]),
    );
  }
}

class _SpecCard extends StatelessWidget {
  const _SpecCard({required this.k, required this.v, this.mono = true});
  final String k, v;
  final bool mono;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Container(
      width: 260,
      padding: const EdgeInsets.fromLTRB(15, 13, 15, 14),
      decoration: BoxDecoration(
        color: c.bgCard, border: Border.all(color: c.borderCard), borderRadius: BorderRadius.circular(6),
        boxShadow: [BoxShadow(color: Colors.black.withValues(alpha: 0.04), blurRadius: 3, offset: const Offset(0, 1))],
      ),
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Text(k.toUpperCase(), style: TextStyle(color: c.textSecondary, fontSize: 10.5, fontWeight: FontWeight.w600, letterSpacing: 0.5)),
        const SizedBox(height: 8),
        SelectableText(v, style: mono ? _mono(c.textPrimary, 13) : TextStyle(color: c.textPrimary, fontSize: 13)),
      ]),
    );
  }
}

class _SectionTitle extends StatelessWidget {
  const _SectionTitle(this.text);
  final String text;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(0, 20, 0, 10),
      child: Text(text.toUpperCase(),
          style: TextStyle(color: c.textSecondary, fontSize: 11, fontWeight: FontWeight.w700, letterSpacing: 0.7)),
    );
  }
}

class _NavItem extends StatelessWidget {
  const _NavItem({required this.icon, required this.label, required this.onTap});
  final IconData icon;
  final String label;
  final VoidCallback onTap;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return InkWell(
      onTap: onTap,
      borderRadius: BorderRadius.circular(6),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
        child: Row(children: [
          Icon(icon, size: 16, color: c.textSecondary),
          const SizedBox(width: 9),
          Text(label, style: TextStyle(color: c.textSecondary, fontSize: 12.5, fontWeight: FontWeight.w500)),
        ]),
      ),
    );
  }
}

class _MiniBadge extends StatelessWidget {
  const _MiniBadge(this.text, this.bg, this.fg);
  final String text;
  final Color bg, fg;
  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
      decoration: BoxDecoration(color: bg, borderRadius: BorderRadius.circular(999)),
      child: Text(text, style: TextStyle(color: fg, fontSize: 10.5, fontWeight: FontWeight.w600)),
    );
  }
}
