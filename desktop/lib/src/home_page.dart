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

/// Which pane the main area shows. Selected from the sidebar (there is no rail).
enum NavView { connection, overview, settings }

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

String _fmtSize(int b) {
  if (b < 1024) return '$b B';
  final kb = b / 1024;
  if (kb < 1024) return '${kb.toStringAsFixed(kb < 10 ? 1 : 0)} KB';
  final mb = kb / 1024;
  return '${mb.toStringAsFixed(mb < 10 ? 1 : 0)} MB';
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
  NavView _view = NavView.connection;
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
          view: _view,
          listWidth: _listWidth,
          activeTab: _tab,
          onResize: (dx) => setState(() => _listWidth = (_listWidth + dx).clamp(200.0, 460.0)),
          onSelectTab: (t) => setState(() => _tab = t),
          onSelectConn: (folder) => setState(() {
            state.select(folder);
            _view = NavView.connection;
          }),
          onSelectFleet: () => setState(() => _view = NavView.overview),
          onSelectSettings: () => setState(() => _view = NavView.settings),
          onAdd: _addConnection,
          onOpen: _openExisting,
          onSync: (i) => state.sync(i),
          onSyncAll: _syncAll,
          onEdit: _editConnection,
          onReveal: _reveal,
          onRemove: _confirmRemove,
          onRevealDir: (path) => _run(() => state.reveal(path)),
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

/// The MDH-style shell: one persistent sidebar + the active pane. Public and
/// callback-driven so it renders in golden tests from seeded state, with no
/// bridge dependence.
class MdhScaffold extends StatelessWidget {
  const MdhScaffold({
    super.key,
    required this.state,
    this.view = NavView.connection,
    this.listWidth = 250,
    this.activeTab = 'overview',
    this.onResize,
    this.onSelectTab,
    this.onSelectConn,
    this.onSelectFleet,
    this.onSelectSettings,
    this.onAdd,
    this.onOpen,
    this.onSync,
    this.onSyncAll,
    this.onEdit,
    this.onReveal,
    this.onRemove,
    this.onRevealDir,
    this.onChooseParent,
    this.onAbout,
    this.onCheckUpdate,
  });

  final AppState state;
  final NavView view;
  final double listWidth;
  final String activeTab;
  final void Function(double)? onResize;
  final void Function(String)? onSelectTab;
  final void Function(String folder)? onSelectConn;
  final VoidCallback? onSelectFleet;
  final VoidCallback? onSelectSettings;
  final VoidCallback? onAdd;
  final VoidCallback? onOpen;
  final void Function(ConnItem)? onSync;
  final VoidCallback? onSyncAll;
  final void Function(ConnItem)? onEdit;
  final void Function(ConnItem)? onReveal;
  final void Function(ConnItem)? onRemove;
  final void Function(String path)? onRevealDir;
  final VoidCallback? onChooseParent;
  final VoidCallback? onAbout;
  final VoidCallback? onCheckUpdate;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);

    // No connections folder yet → full-screen onboarding, no sidebar.
    if (state.parentFolder == null) {
      return Container(
        color: c.bgBase,
        child: _ChooseFolderEmpty(onChoose: onChooseParent ?? () {}),
      );
    }

    final main = switch (view) {
      NavView.connection => _ConnMain(
          state: state,
          activeTab: activeTab,
          onSelectTab: onSelectTab ?? (_) {},
          onSync: onSync ?? (_) {},
          onEdit: onEdit ?? (_) {},
          onReveal: onReveal ?? (_) {},
          onRemove: onRemove ?? (_) {},
          onRevealDir: onRevealDir ?? (_) {},
          onAdd: onAdd ?? () {},
        ),
      NavView.overview => _FleetView(
          state: state,
          onNew: onAdd ?? () {},
          onSyncAll: onSyncAll ?? () {},
          onOpenConn: onSelectConn ?? (_) {},
        ),
      NavView.settings => _SettingsView(
          state: state,
          onChooseParent: onChooseParent ?? () {},
          onOpen: onOpen ?? () {},
          onAbout: onAbout ?? () {},
          onCheckUpdate: onCheckUpdate ?? () {},
        ),
    };

    return Container(
      color: c.bgBase,
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          SizedBox(
            width: listWidth,
            child: _Sidebar(
              state: state,
              view: view,
              onSelect: onSelectConn ?? (_) {},
              onSelectFleet: onSelectFleet ?? () {},
              onSelectSettings: onSelectSettings ?? () {},
              onAdd: onAdd ?? () {},
              onOpen: onOpen ?? () {},
            ),
          ),
          _Resizer(onDelta: onResize ?? (_) {}),
          Expanded(child: main),
        ],
      ),
    );
  }
}

// ------------------------------------------------------------ sidebar

class _Sidebar extends StatelessWidget {
  const _Sidebar({
    required this.state,
    required this.view,
    required this.onSelect,
    required this.onSelectFleet,
    required this.onSelectSettings,
    required this.onAdd,
    required this.onOpen,
  });
  final AppState state;
  final NavView view;
  final void Function(String) onSelect;
  final VoidCallback onSelectFleet, onSelectSettings, onAdd, onOpen;

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
          // brand + settings
          Padding(
            padding: const EdgeInsets.fromLTRB(14, 12, 8, 8),
            child: Row(
              children: [
                Text('rdc', style: TextStyle(color: c.accent, fontSize: 16, fontWeight: FontWeight.w800, letterSpacing: -0.3)),
                const Spacer(),
                Tooltip(
                  message: 'Settings',
                  child: InkWell(
                    onTap: onSelectSettings,
                    borderRadius: BorderRadius.circular(6),
                    child: Container(
                      width: 28,
                      height: 28,
                      decoration: BoxDecoration(
                        color: view == NavView.settings ? c.accent : Colors.transparent,
                        borderRadius: BorderRadius.circular(6),
                      ),
                      child: Icon(Icons.settings_outlined, size: 17,
                          color: view == NavView.settings ? Colors.white : c.textSecondary),
                    ),
                  ),
                ),
              ],
            ),
          ),
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 8),
            child: _NavItem(
              icon: Icons.dashboard_outlined,
              label: 'Fleet overview',
              selected: view == NavView.overview,
              onTap: onSelectFleet,
            ),
          ),
          Padding(
            padding: const EdgeInsets.fromLTRB(14, 14, 10, 6),
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
                      for (final it in state.connections)
                        _SidebarRow(state: state, item: it, active: view == NavView.connection, onSelect: onSelect),
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
  const _SidebarRow({required this.state, required this.item, required this.active, required this.onSelect});
  final AppState state;
  final ConnItem item;
  final bool active;
  final void Function(String) onSelect;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final s = item.summary;
    final sel = active && s.folder == state.selectedFolder;
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

// ------------------------------------------------------------ connection pane

class _ConnMain extends StatelessWidget {
  const _ConnMain({
    required this.state,
    required this.activeTab,
    required this.onSelectTab,
    required this.onSync,
    required this.onEdit,
    required this.onReveal,
    required this.onRemove,
    required this.onRevealDir,
    required this.onAdd,
  });
  final AppState state;
  final String activeTab;
  final void Function(String) onSelectTab;
  final void Function(ConnItem) onSync, onEdit, onReveal, onRemove;
  final void Function(String) onRevealDir;
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
        _TabBar(
          active: activeTab,
          tabs: const ['overview', 'log', 'files'],
          labels: const {'overview': 'Overview', 'log': 'Sync log', 'files': 'Files'},
          onSelect: onSelectTab,
        ),
        Expanded(
          child: switch (activeTab) {
            'files' => Padding(
                padding: const EdgeInsets.all(18),
                child: _FilesPanel(
                  key: ValueKey('files:${item.summary.folder}'),
                  rootFolder: item.summary.folder,
                  revision: item.summary.fileCount.toInt(),
                  onRevealDir: onRevealDir,
                ),
              ),
            'log' => SingleChildScrollView(
                padding: const EdgeInsets.all(18),
                child: _SyncLogCard(state: state, item: item, expanded: true),
              ),
            _ => SingleChildScrollView(
                padding: const EdgeInsets.all(18),
                child: _OverviewPanel(state: state, item: item, st: st),
              ),
          },
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
        _CardGrid(children: [
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

// ------------------------------------------------------------ files pane

class _FEntry {
  const _FEntry({required this.name, required this.isDir, required this.size, required this.count});
  final String name;
  final bool isDir;
  final int size; // bytes, files only
  final int count; // child entries, dirs only
}

/// Finder-style browser of a connection's folder on disk. Reads directly with
/// dart:io (the folder is the CLI's own snapshot), navigating into subfolders
/// with a Back button and a clickable breadcrumb. Dotfiles are hidden.
class _FilesPanel extends StatefulWidget {
  const _FilesPanel({super.key, required this.rootFolder, required this.revision, required this.onRevealDir});
  final String rootFolder;
  final int revision; // reload trigger (grows after a sync)
  final void Function(String absPath) onRevealDir;
  @override
  State<_FilesPanel> createState() => _FilesPanelState();
}

class _FilesPanelState extends State<_FilesPanel> {
  List<String> _crumbs = [];
  List<_FEntry> _entries = const [];
  String? _error;

  String get _sep => Platform.pathSeparator;
  String get _absPath => [widget.rootFolder, ..._crumbs].join(_sep);

  @override
  void initState() {
    super.initState();
    _readInto();
  }

  @override
  void didUpdateWidget(covariant _FilesPanel old) {
    super.didUpdateWidget(old);
    if (old.rootFolder != widget.rootFolder) {
      _crumbs = [];
      _readInto();
    } else if (old.revision != widget.revision) {
      _readInto();
    }
  }

  bool _hidden(String name) => name.startsWith('.');

  void _readInto() {
    try {
      final list = Directory(_absPath).listSync(followLinks: false);
      final out = <_FEntry>[];
      for (final e in list) {
        final name = e.path.split(_sep).last;
        if (_hidden(name)) continue;
        if (e is Directory) {
          var n = 0;
          try {
            n = e.listSync(followLinks: false).where((x) => !_hidden(x.path.split(_sep).last)).length;
          } catch (_) {}
          out.add(_FEntry(name: name, isDir: true, size: 0, count: n));
        } else if (e is File) {
          var sz = 0;
          try {
            sz = e.lengthSync();
          } catch (_) {}
          out.add(_FEntry(name: name, isDir: false, size: sz, count: 0));
        }
      }
      out.sort((a, b) =>
          a.isDir != b.isDir ? (a.isDir ? -1 : 1) : a.name.toLowerCase().compareTo(b.name.toLowerCase()));
      _entries = out;
      _error = null;
    } catch (e) {
      _entries = const [];
      _error = errorText(e);
    }
  }

  void _go(List<String> crumbs) => setState(() {
        _crumbs = crumbs;
        _readInto();
      });

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final rootName = widget.rootFolder.split(_sep).last;
    final segs = [rootName, ..._crumbs];

    final crumbs = <Widget>[];
    for (var i = 0; i < segs.length; i++) {
      final cur = i == segs.length - 1;
      crumbs.add(InkWell(
        onTap: cur ? null : () => _go(_crumbs.sublist(0, i)),
        borderRadius: BorderRadius.circular(4),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 2, vertical: 2),
          child: Text(segs[i], style: _mono(cur ? c.textPrimary : c.textSecondary, 12.5, cur ? FontWeight.w600 : FontWeight.w400)),
        ),
      ));
      if (!cur) crumbs.add(Padding(padding: const EdgeInsets.symmetric(horizontal: 3), child: Text('›', style: _mono(c.textHint, 12.5))));
    }

    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Row(
          children: [
            Opacity(
              opacity: _crumbs.isEmpty ? 0.45 : 1,
              child: InkWell(
                onTap: _crumbs.isEmpty ? null : () => _go(_crumbs.sublist(0, _crumbs.length - 1)),
                borderRadius: BorderRadius.circular(6),
                child: Container(
                  width: 28,
                  height: 28,
                  decoration: BoxDecoration(color: c.bgCard, border: Border.all(color: c.border), borderRadius: BorderRadius.circular(6)),
                  child: Icon(Icons.chevron_left, size: 19, color: c.textPrimary),
                ),
              ),
            ),
            const SizedBox(width: 10),
            Expanded(child: Wrap(crossAxisAlignment: WrapCrossAlignment.center, children: crumbs)),
            const SizedBox(width: 10),
            _Btn(label: 'Reveal in Finder', onTap: () => widget.onRevealDir(_absPath)),
          ],
        ),
        const SizedBox(height: 12),
        Expanded(
          child: Container(
            decoration: BoxDecoration(
              color: c.bgCard,
              border: Border.all(color: c.borderCard),
              borderRadius: BorderRadius.circular(6),
              boxShadow: [BoxShadow(color: Colors.black.withValues(alpha: 0.04), blurRadius: 3, offset: const Offset(0, 1))],
            ),
            clipBehavior: Clip.antiAlias,
            child: Column(
              children: [
                Container(
                  decoration: BoxDecoration(color: c.bgSidebar, border: Border(bottom: BorderSide(color: c.border))),
                  padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 9),
                  child: Row(children: [
                    Expanded(child: Text('NAME', style: TextStyle(color: c.textSecondary, fontSize: 10.5, fontWeight: FontWeight.w600, letterSpacing: 0.5))),
                    Text('SIZE', style: TextStyle(color: c.textSecondary, fontSize: 10.5, fontWeight: FontWeight.w600, letterSpacing: 0.5)),
                  ]),
                ),
                Expanded(child: _list(c)),
              ],
            ),
          ),
        ),
      ],
    );
  }

  Widget _list(MdhColors c) {
    if (_error != null) {
      return Center(
        child: Padding(
          padding: const EdgeInsets.all(24),
          child: SelectableText("Couldn't read this folder.\n$_error",
              textAlign: TextAlign.center, style: TextStyle(color: c.textSecondary, fontSize: 12.5)),
        ),
      );
    }
    if (_entries.isEmpty) {
      return Center(child: Text('This folder is empty.', style: TextStyle(color: c.textSecondary, fontSize: 12.5)));
    }
    return ListView.builder(
      itemCount: _entries.length,
      itemBuilder: (context, i) {
        final e = _entries[i];
        final last = i == _entries.length - 1;
        return InkWell(
          onTap: e.isDir ? () => _go([..._crumbs, e.name]) : null,
          child: Container(
            decoration: BoxDecoration(border: last ? null : Border(bottom: BorderSide(color: c.border))),
            padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 9),
            child: Row(children: [
              Icon(e.isDir ? Icons.folder_rounded : Icons.insert_drive_file_outlined,
                  size: 18, color: e.isDir ? c.accent : c.textSecondary),
              const SizedBox(width: 12),
              Expanded(child: Text(e.name, overflow: TextOverflow.ellipsis, style: _mono(c.textPrimary, 13, FontWeight.w500))),
              const SizedBox(width: 12),
              Text(e.isDir ? '${e.count} item${e.count == 1 ? '' : 's'}' : _fmtSize(e.size), style: _mono(c.textHint, 12)),
            ]),
          ),
        );
      },
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
              _SpecCard(k: 'Connections folder', v: state.parentFolder ?? '(not set)', width: 260),
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

/// Lays cards out in a responsive grid where every card in a row shares one
/// height (the tallest in that row), like CSS `grid` with `align-items: stretch`.
/// Column count adapts to the available width; the last row keeps grid-aligned
/// widths by padding with empty cells rather than stretching its cards.
class _CardGrid extends StatelessWidget {
  const _CardGrid({required this.children});
  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    const minCardWidth = 240.0, gap = 12.0;
    return LayoutBuilder(
      builder: (context, cons) {
        final n = children.length;
        if (n == 0) return const SizedBox.shrink();
        var cols = ((cons.maxWidth + gap) / (minCardWidth + gap)).floor();
        cols = cols.clamp(1, n);
        final rows = <Widget>[];
        for (var i = 0; i < n; i += cols) {
          final end = (i + cols) < n ? i + cols : n;
          final cells = <Widget>[];
          for (var j = 0; j < cols; j++) {
            if (j > 0) cells.add(SizedBox(width: gap));
            final idx = i + j;
            cells.add(Expanded(child: idx < end ? children[idx] : const SizedBox.shrink()));
          }
          if (rows.isNotEmpty) rows.add(SizedBox(height: gap));
          rows.add(IntrinsicHeight(
            child: Row(crossAxisAlignment: CrossAxisAlignment.stretch, children: cells),
          ));
        }
        return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: rows);
      },
    );
  }
}

class _SpecCard extends StatelessWidget {
  const _SpecCard({required this.k, required this.v, this.mono = true, this.width});
  final String k, v;
  final bool mono;

  /// Fixed width for standalone use. Null lets the card fill its parent (e.g.
  /// an Expanded cell in [_CardGrid]), so a row of cards shares one width/height.
  final double? width;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Container(
      width: width,
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
  const _NavItem({required this.icon, required this.label, required this.onTap, this.selected = false});
  final IconData icon;
  final String label;
  final VoidCallback onTap;
  final bool selected;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final fg = selected ? Colors.white : c.textSecondary;
    return InkWell(
      onTap: onTap,
      borderRadius: BorderRadius.circular(6),
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
        decoration: BoxDecoration(
          color: selected ? c.accent : Colors.transparent,
          borderRadius: BorderRadius.circular(6),
        ),
        child: Row(children: [
          Icon(icon, size: 16, color: fg),
          const SizedBox(width: 9),
          Text(label, style: TextStyle(color: fg, fontSize: 12.5, fontWeight: FontWeight.w500)),
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
