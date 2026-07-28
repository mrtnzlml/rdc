import 'dart:io';

import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'ansi.dart';
import 'app_state.dart';
import 'console_theme.dart';
import 'dialogs.dart';
import 'error_text.dart';
import 'rust/api/rdc.dart';
import 'update_check.dart';

// ------------------------------------------------------------ status helpers

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

String _relTime(int? unix) {
  if (unix == null) return 'never';
  final then = DateTime.fromMillisecondsSinceEpoch(unix * 1000);
  final d = DateTime.now().difference(then);
  if (d.inSeconds < 60) return 'now';
  if (d.inMinutes < 60) return '${d.inMinutes}m';
  if (d.inHours < 24) return '${d.inHours}h';
  return '${d.inDays}d';
}

// ------------------------------------------------------------ page

class HomePage extends StatefulWidget {
  const HomePage({super.key, required this.state});
  final AppState state;

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
  final FocusNode _focus = FocusNode(debugLabel: 'rdc-console');
  double _listWidth = 200;
  AppState get state => widget.state;

  @override
  void initState() {
    super.initState();
    if (state.parentFolder != null) state.reload();
    _checkUpdate();
  }

  @override
  void dispose() {
    _focus.dispose();
    super.dispose();
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

  void _refocus() {
    if (mounted) _focus.requestFocus();
  }

  Future<void> _chooseParent() async {
    await _run(() async {
      final path = await getDirectoryPath(confirmButtonText: 'Choose');
      if (path != null) await state.setParentFolder(path);
    });
    _refocus();
  }

  Future<void> _openExisting() async {
    await _run(() async {
      final path = await getDirectoryPath(confirmButtonText: 'Open');
      if (path != null) await state.openExisting(path);
    });
    _refocus();
  }

  Future<void> _addConnection() async {
    await showDialog<bool>(context: context, builder: (_) => AddConnectionDialog(state: state));
    _refocus();
  }

  Future<void> _editConnection(ConnItem item) async {
    await showDialog<bool>(context: context, builder: (_) => EditConnectionDialog(state: state, item: item));
    _refocus();
  }

  Future<void> _reveal(ConnItem item) async {
    await _run(() => state.reveal(item.summary.folder));
    _refocus();
  }

  Future<void> _confirmRemove(ConnItem item) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => RemoveDialog(item: item),
    );
    if (ok == true) await _run(() => state.removeOrDetach(item));
    _refocus();
  }

  void _about() {
    showAboutDialog(
      context: context,
      applicationName: 'rdc',
      applicationVersion: 'v$kAppVersion  •  rdc core embedded',
      children: const [
        Text('Cross-platform desktop front-end for the rdc core '
            '(Flutter + flutter_rust_bridge).'),
      ],
    );
  }

  // ---- keyboard --------------------------------------------------------

  void _move(int delta) {
    final list = state.connections;
    if (list.isEmpty) return;
    final i = list.indexWhere((c) => c.summary.folder == state.selectedFolder);
    final next = i < 0 ? 0 : (i + delta).clamp(0, list.length - 1);
    state.select(list[next].summary.folder);
  }

  KeyEventResult _onKey(FocusNode node, KeyEvent e) {
    if (e is! KeyDownEvent) return KeyEventResult.ignored;
    final key = e.logicalKey;
    if (HardwareKeyboard.instance.isMetaPressed && key == LogicalKeyboardKey.keyK) {
      _openPalette();
      return KeyEventResult.handled;
    }
    if (HardwareKeyboard.instance.isControlPressed && key == LogicalKeyboardKey.keyK) {
      _openPalette();
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.keyJ || key == LogicalKeyboardKey.arrowDown) {
      _move(1);
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.keyK || key == LogicalKeyboardKey.arrowUp) {
      _move(-1);
      return KeyEventResult.handled;
    }
    final sel = state.selected;
    if (sel == null) return KeyEventResult.ignored;
    if (key == LogicalKeyboardKey.keyS) {
      state.sync(sel);
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.keyE) {
      _editConnection(sel);
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.keyR) {
      _reveal(sel);
      return KeyEventResult.handled;
    }
    if (key == LogicalKeyboardKey.keyX) {
      _confirmRemove(sel);
      return KeyEventResult.handled;
    }
    return KeyEventResult.ignored;
  }

  Future<void> _openPalette() async {
    final sel = state.selected;
    final cmds = <PaletteCmd>[
      PaletteCmd('New connection', run: _addConnection),
      PaletteCmd('Open existing project', run: _openExisting),
      PaletteCmd('Change parent folder…', run: _chooseParent),
      PaletteCmd('About rdc', run: _about),
      if (sel != null) ...[
        PaletteCmd('Sync ${sel.summary.name}', key: 's', run: () => state.sync(sel)),
        PaletteCmd('Edit ${sel.summary.name}', key: 'e', run: () => _editConnection(sel)),
        PaletteCmd('Reveal ${sel.summary.name}', key: 'r', run: () => _reveal(sel)),
        PaletteCmd(sel.isExternal ? 'Detach ${sel.summary.name}' : 'Remove ${sel.summary.name}',
            key: 'x', run: () => _confirmRemove(sel)),
      ],
      for (final c in state.connections)
        PaletteCmd('→ ${c.summary.name}', run: () => state.select(c.summary.folder)),
    ];
    final chosen = await showDialog<PaletteCmd>(
      context: context,
      builder: (_) => CommandPalette(commands: cmds),
    );
    _refocus();
    chosen?.run();
  }

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    return Scaffold(
      backgroundColor: c.bg,
      body: ListenableBuilder(
        listenable: state,
        builder: (context, _) {
          if (state.parentFolder == null) {
            return _EmptyState(onChoose: _chooseParent);
          }
          return Focus(
            focusNode: _focus,
            autofocus: true,
            onKeyEvent: _onKey,
            child: ConsoleMain(
              state: state,
              onPalette: _openPalette,
              onSync: (i) => state.sync(i),
              onEdit: _editConnection,
              onReveal: _reveal,
              onRemove: _confirmRemove,
              sidebarWidth: _listWidth,
              onResizeSidebar: (dx) =>
                  setState(() => _listWidth = (_listWidth + dx).clamp(160.0, 480.0)),
            ),
          );
        },
      ),
    );
  }
}

// ------------------------------------------------------------ main scaffold

/// The connected main screen: command bar + connection list + record + footer.
/// Public and callback-driven so it renders in golden tests from seeded state,
/// independent of the bridge or the initial reload.
class ConsoleMain extends StatelessWidget {
  const ConsoleMain({
    super.key,
    required this.state,
    this.onPalette,
    this.onSync,
    this.onEdit,
    this.onReveal,
    this.onRemove,
    this.sidebarWidth = 200,
    this.onResizeSidebar,
  });
  final AppState state;
  final VoidCallback? onPalette;
  final void Function(ConnItem)? onSync;
  final void Function(ConnItem)? onEdit;
  final void Function(ConnItem)? onReveal;
  final void Function(ConnItem)? onRemove;
  final double sidebarWidth;
  final ValueChanged<double>? onResizeSidebar;

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        _CommandBar(state: state, onPalette: onPalette ?? () {}),
        Expanded(
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              SizedBox(width: sidebarWidth, child: _ConnList(state: state)),
              _Resizer(onDelta: onResizeSidebar ?? (_) {}),
              Expanded(
                child: _Record(
                  state: state,
                  onSync: onSync ?? (_) {},
                  onEdit: onEdit ?? (_) {},
                  onReveal: onReveal ?? (_) {},
                  onRemove: onRemove ?? (_) {},
                ),
              ),
            ],
          ),
        ),
        const _Footer(),
      ],
    );
  }
}

// ------------------------------------------------------------ resizer

/// Draggable divider between the list and the record: a 1px line inside an 8px
/// hit area, with a horizontal-resize cursor.
class _Resizer extends StatelessWidget {
  const _Resizer({required this.onDelta});
  final ValueChanged<double> onDelta;

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    return MouseRegion(
      cursor: SystemMouseCursors.resizeLeftRight,
      child: GestureDetector(
        behavior: HitTestBehavior.translucent,
        onHorizontalDragUpdate: (d) => onDelta(d.delta.dx),
        child: SizedBox(
          width: 8,
          child: Center(child: Container(width: 1, color: c.line)),
        ),
      ),
    );
  }
}

// ------------------------------------------------------------ command bar

class _CommandBar extends StatelessWidget {
  const _CommandBar({required this.state, required this.onPalette});
  final AppState state;
  final VoidCallback onPalette;

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    final sel = state.selected;
    Widget context0;
    Widget? chip;
    if (sel != null) {
      context0 = RichText(
        text: TextSpan(
          style: TextStyle(color: c.muted, fontSize: 13),
          children: [
            const TextSpan(text: 'connection '),
            TextSpan(text: sel.summary.name, style: TextStyle(color: c.ink, fontWeight: FontWeight.w600)),
          ],
        ),
      );
      chip = _StatusChip(state: state, item: sel);
    } else {
      final n = state.connections.length;
      context0 = Text('${state.parentFolder}  ·  $n connection${n == 1 ? '' : 's'}',
          style: TextStyle(color: c.muted, fontSize: 13));
    }
    return Container(
      padding: const EdgeInsets.fromLTRB(16, 12, 14, 12),
      decoration: BoxDecoration(
        color: c.surf,
        border: Border(bottom: BorderSide(color: c.line)),
        gradient: LinearGradient(
          begin: Alignment.topCenter,
          end: Alignment.bottomCenter,
          colors: [c.acc.withValues(alpha: 0.08), Colors.transparent],
        ),
      ),
      child: Row(
        children: [
          Text('rdc ▸', style: TextStyle(color: c.acc, fontWeight: FontWeight.w700, fontSize: 13)),
          const SizedBox(width: 10),
          // Left group takes all slack so ⌘K is always flush right.
          Expanded(
            child: Row(
              children: [
                Flexible(child: context0),
                if (chip != null) ...[const SizedBox(width: 12), chip],
              ],
            ),
          ),
          const SizedBox(width: 12),
          _KbdHint(onTap: onPalette, label: '⌘K'),
        ],
      ),
    );
  }
}

class _StatusChip extends StatelessWidget {
  const _StatusChip({required this.state, required this.item});
  final AppState state;
  final ConnItem item;

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    final (String label, Color color) = switch (_statusOf(state, item)) {
      _St.running => ('● syncing', c.acc),
      _St.error => ('✕ sync failed', c.err),
      _St.synced => ('● synced', c.ok),
      _St.never => ('○ never synced', c.muted),
    };
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 9, vertical: 3),
      decoration: BoxDecoration(
        borderRadius: BorderRadius.circular(999),
        border: Border.all(color: Color.alphaBlend(color.withValues(alpha: 0.4), c.line)),
      ),
      child: Text(label, style: TextStyle(color: color, fontSize: 11)),
    );
  }
}

class _KbdHint extends StatelessWidget {
  const _KbdHint({required this.label, this.onTap});
  final String label;
  final VoidCallback? onTap;
  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    return InkWell(
      onTap: onTap,
      borderRadius: BorderRadius.circular(5),
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 7, vertical: 3),
        decoration: BoxDecoration(
          border: Border.all(color: c.line),
          borderRadius: BorderRadius.circular(5),
        ),
        child: Text(label, style: TextStyle(color: c.muted, fontSize: 11)),
      ),
    );
  }
}

// ------------------------------------------------------------ connection list

class _ConnList extends StatelessWidget {
  const _ConnList({required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    if (state.connections.isEmpty) {
      return Center(
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Text('no connections\n⌘K → new',
              textAlign: TextAlign.center, style: TextStyle(color: c.muted, fontSize: 12)),
        ),
      );
    }
    return ListView.builder(
      padding: const EdgeInsets.symmetric(vertical: 8),
      itemCount: state.connections.length,
      itemBuilder: (context, i) => _ConnRow(state: state, item: state.connections[i]),
    );
  }
}

class _ConnRow extends StatelessWidget {
  const _ConnRow({required this.state, required this.item});
  final AppState state;
  final ConnItem item;

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    final s = item.summary;
    final selected = s.folder == state.selectedFolder;
    final st = _statusOf(state, item);

    Widget glyph;
    switch (st) {
      case _St.running:
        glyph = SizedBox(
            width: 12, height: 12, child: CircularProgressIndicator(strokeWidth: 1.6, color: c.acc));
      case _St.error:
        glyph = Text('✕', style: TextStyle(color: c.err, fontSize: 12));
      case _St.synced:
        glyph = Text('●', style: TextStyle(color: c.ok, fontSize: 12));
      case _St.never:
        glyph = Text('○', style: TextStyle(color: c.muted, fontSize: 12));
    }

    final sub = switch (st) {
      _St.running => 'syncing…',
      _St.error => 'failed',
      _St.synced => _relTime(s.lastSyncUnix),
      _St.never => 'never',
    };
    final ext = item.isExternal ? ' · ext' : '';

    return InkWell(
      onTap: () => state.select(s.folder),
      child: Container(
        decoration: BoxDecoration(
          color: selected ? c.sel : Colors.transparent,
          border: Border(left: BorderSide(color: selected ? c.acc : Colors.transparent, width: 2)),
        ),
        padding: const EdgeInsets.fromLTRB(12, 9, 12, 9),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            SizedBox(width: 15, child: Align(alignment: Alignment.centerLeft, child: glyph)),
            const SizedBox(width: 8),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(s.name, style: TextStyle(color: c.ink, fontSize: 13), overflow: TextOverflow.ellipsis),
                  const SizedBox(height: 1),
                  Text('org ${s.orgId} · $sub$ext',
                      style: TextStyle(color: c.muted, fontSize: 11), overflow: TextOverflow.ellipsis),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

// ------------------------------------------------------------ record (detail)

class _Record extends StatelessWidget {
  const _Record({
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
    final c = ConsoleColors.of(context);
    final item = state.selected;
    if (item == null) {
      return Center(
        child: Text('rdc ▸ select a connection', style: TextStyle(color: c.muted, fontSize: 13)),
      );
    }
    final s = item.summary;
    final st = _statusOf(state, item);
    final auth = s.authKind == AuthKind.token ? 'api_token' : 'username & password';

    return SingleChildScrollView(
      padding: const EdgeInsets.fromLTRB(20, 18, 20, 18),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          // header
          Row(
            children: [
              Text('◆ ', style: TextStyle(color: st == _St.error ? c.err : c.acc, fontSize: 18)),
              Flexible(
                child: Text(s.name,
                    style: TextStyle(color: c.ink, fontSize: 19, fontWeight: FontWeight.w700)),
              ),
              const SizedBox(width: 9),
              _Badge(text: item.isExternal ? 'external' : 'managed'),
            ],
          ),
          const SizedBox(height: 16),
          // kv
          _Kv(rows: [
            ('api_base', s.apiBase, null),
            ('org_id', s.orgId.toString(), null),
            ('auth', auth, null),
            ('folder', s.folder, null),
            ('files', s.fileCount.toString(), null),
            ('last_sync', _lastSyncText(st, s), _lastSyncColor(context, st)),
          ]),
          const SizedBox(height: 14),
          // actions
          Wrap(spacing: 8, runSpacing: 8, children: [
            _CBtn(keyChar: 's', label: st == _St.error ? 'retry' : 'ync',
                primary: true, onTap: st == _St.running ? null : () => onSync(item)),
            _CBtn(keyChar: 'e', label: 'dit', onTap: () => onEdit(item)),
            _CBtn(keyChar: 'r', label: 'eveal', onTap: () => onReveal(item)),
            _CBtn(keyChar: 'x', label: item.isExternal ? ' detach' : ' remove', onTap: () => onRemove(item)),
          ]),
          const SizedBox(height: 16),
          _SyncLog(state: state, item: item),
        ],
      ),
    );
  }

  String _lastSyncText(_St st, ConnectionSummary s) => switch (st) {
        _St.running => 'syncing…',
        _St.error => 'failed · ${_relTime(s.lastSyncUnix)}',
        _St.synced => 'ok · ${_relTime(s.lastSyncUnix)}',
        _St.never => 'never',
      };

  Color? _lastSyncColor(BuildContext ctx, _St st) {
    final c = ConsoleColors.of(ctx);
    return switch (st) {
      _St.error => c.err,
      _St.synced => c.ok,
      _ => null,
    };
  }
}

class _Badge extends StatelessWidget {
  const _Badge({required this.text});
  final String text;
  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
      decoration: BoxDecoration(
        border: Border.all(color: c.line),
        borderRadius: BorderRadius.circular(4),
      ),
      child: Text(text.toUpperCase(),
          style: TextStyle(color: c.muted, fontSize: 10, letterSpacing: 0.6)),
    );
  }
}

class _Kv extends StatelessWidget {
  const _Kv({required this.rows});
  final List<(String, String, Color?)> rows;
  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        for (final (k, v, color) in rows)
          Padding(
            padding: const EdgeInsets.only(bottom: 7),
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                SizedBox(
                  width: 92,
                  child: RichText(
                    text: TextSpan(style: TextStyle(color: c.muted, fontSize: 13), children: [
                      TextSpan(text: '› ', style: TextStyle(color: c.acc)),
                      TextSpan(text: k),
                    ]),
                  ),
                ),
                const SizedBox(width: 18),
                Expanded(
                  child: SelectableText(v,
                      style: TextStyle(color: color ?? c.ink, fontSize: 13)),
                ),
              ],
            ),
          ),
      ],
    );
  }
}

class _CBtn extends StatelessWidget {
  const _CBtn({required this.keyChar, required this.label, this.primary = false, this.onTap});
  final String keyChar;
  final String label;
  final bool primary;
  final VoidCallback? onTap;

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    return InkWell(
      onTap: onTap,
      borderRadius: BorderRadius.circular(6),
      child: Opacity(
        opacity: onTap == null ? 0.5 : 1,
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
          decoration: BoxDecoration(
            color: primary ? c.acc.withValues(alpha: 0.08) : Colors.transparent,
            border: Border.all(color: primary ? c.acc : c.line),
            borderRadius: BorderRadius.circular(6),
          ),
          child: RichText(
            text: TextSpan(
              style: TextStyle(fontSize: 12, fontWeight: FontWeight.w600, color: primary ? c.acc : c.ink),
              children: [
                TextSpan(text: keyChar, style: TextStyle(color: c.acc)),
                TextSpan(text: label),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

class _SyncLog extends StatelessWidget {
  const _SyncLog({required this.state, required this.item});
  final AppState state;
  final ConnItem item;

  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    final st = _statusOf(state, item);
    final msg = state.syncMessage[item.summary.folder];
    final lines = state.syncLog[item.summary.folder] ?? const <String>[];

    Widget body;
    if (lines.isNotEmpty) {
      // rdc's real, rendered log (ANSI-colored) — full width, scrollable,
      // latest line in view.
      final spans = <InlineSpan>[];
      for (var k = 0; k < lines.length; k++) {
        spans.addAll(ansiSpans(lines[k], c, 12.5));
        if (k < lines.length - 1) spans.add(const TextSpan(text: '\n'));
      }
      body = SizedBox(
        width: double.infinity,
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxHeight: 168),
          child: Scrollbar(
            child: SingleChildScrollView(
              reverse: true,
              child: SelectableText.rich(
                TextSpan(style: const TextStyle(fontSize: 12.5, height: 1.5), children: spans),
              ),
            ),
          ),
        ),
      );
    } else {
      // No live log this session: one-line summary from the current state.
      final (String prefix, Color pc, String text, Color tc) = switch (st) {
        _St.running => ('», ', c.acc, 'syncing…', c.ink),
        _St.error => ('✕ ', c.err, msg ?? 'sync failed', c.err),
        _St.synced => ('✓ ', c.ok, msg ?? 'up to date · ${_relTime(item.summary.lastSyncUnix)}', c.ok),
        _St.never => ('— ', c.muted, 'not synced yet', c.muted),
      };
      body = SelectableText.rich(TextSpan(style: const TextStyle(fontSize: 12.5), children: [
        TextSpan(text: prefix, style: TextStyle(color: pc)),
        TextSpan(text: text, style: TextStyle(color: tc)),
      ]));
    }

    return Container(
      width: double.infinity,
      decoration: BoxDecoration(
        color: c.log,
        border: Border.all(color: c.line),
        borderRadius: BorderRadius.circular(8),
      ),
      padding: const EdgeInsets.fromLTRB(13, 11, 13, 11),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            mainAxisAlignment: MainAxisAlignment.spaceBetween,
            children: [
              Text('SYNC LOG', style: TextStyle(color: c.muted, fontSize: 10.5, letterSpacing: 1)),
              Text('pull-only', style: TextStyle(color: c.muted, fontSize: 10.5, letterSpacing: 1)),
            ],
          ),
          const SizedBox(height: 8),
          body,
        ],
      ),
    );
  }
}

// ------------------------------------------------------------ footer

class _Footer extends StatelessWidget {
  const _Footer();
  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    TextSpan hint(String k, String label) => TextSpan(children: [
          TextSpan(text: k, style: TextStyle(color: c.ink, fontWeight: FontWeight.w700)),
          TextSpan(text: ' $label   ', style: TextStyle(color: c.muted)),
        ]);
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
      decoration: BoxDecoration(
        color: c.surf,
        border: Border(top: BorderSide(color: c.line)),
      ),
      child: RichText(
        text: TextSpan(style: const TextStyle(fontSize: 11), children: [
          hint('j/k', 'move'),
          hint('s', 'sync'),
          hint('e', 'edit'),
          hint('⌘K', 'commands'),
        ]),
      ),
    );
  }
}

// ------------------------------------------------------------ empty state

class _EmptyState extends StatelessWidget {
  const _EmptyState({required this.onChoose});
  final VoidCallback onChoose;
  @override
  Widget build(BuildContext context) {
    final c = ConsoleColors.of(context);
    TextStyle mut = TextStyle(color: c.muted, fontSize: 13, height: 1.7);
    return Center(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 460),
        child: Container(
          margin: const EdgeInsets.all(24),
          padding: const EdgeInsets.all(18),
          decoration: BoxDecoration(
            color: c.log,
            border: Border.all(color: c.line, style: BorderStyle.solid),
            borderRadius: BorderRadius.circular(8),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisSize: MainAxisSize.min,
            children: [
              RichText(
                text: TextSpan(style: TextStyle(fontSize: 13, height: 1.7), children: [
                  TextSpan(text: 'rdc ▸ ', style: TextStyle(color: c.acc, fontWeight: FontWeight.w700)),
                  TextSpan(text: 'no connections folder set', style: TextStyle(color: c.ink)),
                ]),
              ),
              Text('# each connection is a subfolder the CLI shares', style: mut),
              Text('# choose where they live:', style: mut),
              const SizedBox(height: 12),
              InkWell(
                onTap: onChoose,
                borderRadius: BorderRadius.circular(6),
                child: Container(
                  padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
                  decoration: BoxDecoration(
                    color: c.acc.withValues(alpha: 0.08),
                    border: Border.all(color: c.acc),
                    borderRadius: BorderRadius.circular(6),
                  ),
                  child: Text('▸ choose folder…',
                      style: TextStyle(color: c.acc, fontSize: 12, fontWeight: FontWeight.w600)),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
