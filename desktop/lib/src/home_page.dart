import 'dart:convert';
import 'dart:io';

import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';

import 'ansi.dart';
import 'app_state.dart';
import 'dialogs.dart';
import 'error_text.dart';
import 'highlight.dart';
import 'mdh_theme.dart';
import 'rust/api/rdc.dart';
import 'update_check.dart';

// ------------------------------------------------------------ helpers

/// Which pane the main area shows. Selected from the sidebar (there is no rail).
enum NavView { connection, overview, settings }

enum _St { running, watching, error, synced, never }

_St _statusOf(AppState s, ProjectItem it, EnvSummary env) {
  switch (s.syncState[s.envKey(it.summary.folder, env.name)]) {
    case SyncState.running:
      return _St.running;
    case SyncState.error:
      return _St.error;
    default:
      if (s.isWatching(it.summary.folder, env.name)) return _St.watching;
      return env.lastSyncUnix != null ? _St.synced : _St.never;
  }
}

/// Badge (label, background, foreground) for a sync status, shared by
/// `_EnvTableRow` and `_FleetRow` so their status pills stay in lockstep.
(String, Color, Color) _badgeFor(MdhColors c, _St st) => switch (st) {
      _St.error => ('error', c.dangerBg, c.dangerFg),
      _St.never => ('never', c.infoBg, c.infoFg),
      _St.running => ('syncing', c.infoBg, c.infoFg),
      _St.watching => ('watching', c.infoBg, c.accent),
      _St.synced => ('synced', c.successBg, c.successFg),
    };

/// True while a watch's stop is still unwinding: `stopWatchItem` flips
/// `running` to false immediately (so the button can react), but the entry
/// itself lingers in `state.watch` until `SyncPhase.stopped` arrives and
/// clears it. `AppState.isWatching` reads false for that whole window, so a
/// caller that checks only `isWatching` would let a second watch — or a
/// plain Sync — start on top of a subscription that hasn't unwound yet,
/// with both ending up writing into the same `pendingPrompts`/`watch` entry.
bool _isStopping(AppState s, String folder, String env) {
  final w = s.watch[s.envKey(folder, env)];
  return w != null && !w.running;
}

/// Sync is refused for that same window: the Rust side treats a one-shot
/// sync on a watched env as a deliberate displacement (it cancels the
/// watch), so Sync must stay disabled for as long as a watch owns the
/// cycle — watching or mid-stop.
bool _syncBlocked(AppState s, String folder, String env) =>
    s.isWatching(folder, env) || _isStopping(s, folder, env);

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

/// In-flight two-way confirmations, keyed by folder. A project's per-row
/// "Sync all envs" button calls the (gated) per-env sync callback once per
/// env in a tight synchronous loop -- every call fires before any dialog
/// result comes back. Without this cache, each of those calls would
/// independently see `needsTwoWayNotice` still true and pop its own copy of
/// the notice dialog; with it, everyone racing for the same unacknowledged
/// folder awaits the one dialog already on screen.
final Map<String, Future<bool>> _twoWayConfirmsInFlight = {};

/// Shows the one-time notice that Sync writes to Rossum, if `item`'s
/// project hasn't seen it yet, and records the acknowledgement if the user
/// proceeds. Returns true when the caller should go ahead with the sync or
/// watch it was about to start. Pass `forWatch: true` from a Watch call site
/// so the notice's copy names the action that actually triggered it.
///
/// Shared by every call site that starts a two-way cycle -- the top-level
/// Sync/Sync-all wiring in [_HomePageState] and the direct `watchEnvItem`
/// calls in [_ConnBar] and [_EnvTableRow]. Two or more callers racing for
/// the same unacknowledged folder in the same synchronous tick (a
/// project's "Sync all envs" fires its gated per-env callback once per env
/// before any dialog result comes back) share the one dialog already on
/// screen rather than each popping their own. That guarantee is per tick,
/// not per bulk operation: a caller that `await`s this sequentially across
/// several envs (`_syncAll`) sees the in-flight entry cleared as soon as
/// the first dialog resolves, so a decline is *not* remembered between
/// sequential calls here -- a caller that needs "ask at most once per
/// project per bulk run" tracks declines itself (see `_syncAll`).
Future<bool> _confirmTwoWay(BuildContext context, AppState state, ProjectItem item, {bool forWatch = false}) {
  final folder = item.summary.folder;
  if (!state.needsTwoWayNotice(folder)) return Future.value(true);
  return _twoWayConfirmsInFlight[folder] ??= () async {
    try {
      final ok = await showDialog<bool>(
            context: context,
            builder: (_) => TwoWayNoticeDialog(projectName: item.summary.name, forWatch: forWatch),
          ) ??
          false;
      if (ok) state.ackTwoWay(folder);
      return ok;
    } finally {
      _twoWayConfirmsInFlight.remove(folder);
    }
  }();
}

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
  // Guards against stacking a second PromptDialog: set as soon as one is
  // scheduled to show, cleared only after it's dismissed.
  bool _promptOpen = false;
  // Identifies the prompt the open dialog is showing, so a build triggered
  // by some OTHER env's prompt changing doesn't mistake it for this one
  // resolving. Cleared by `onAnswer` before it pops, so the "resolved
  // externally" check below never fires for an answer the user just gave
  // (see the check's own comment).
  String? _openPromptKey;
  BigInt? _openPromptId;

  @override
  void initState() {
    super.initState();
    if (state.parentFolder != null) state.reload();
    _checkUpdate();
  }

  Future<void> _checkUpdate() async {
    if (Platform.environment.containsKey('FLUTTER_TEST')) return;
    final info = await checkForUpdate(await rdcVersion() ?? '');
    if (info != null && mounted) {
      ScaffoldMessenger.of(context).showSnackBar(_updateSnackBar(info));
    }
  }

  SnackBar _updateSnackBar(UpdateInfo info) => SnackBar(
        content: Text('A newer version (${info.latest}) is available on GitHub Releases.'),
        duration: const Duration(seconds: 8),
        action: info.url.isEmpty
            ? null
            : SnackBarAction(label: 'Open', onPressed: () => _openUrl(info.url)),
      );

  /// Opens [url] in the default browser; best-effort, failures are ignored.
  void _openUrl(String url) {
    final (cmd, args) = Platform.isMacOS
        ? ('open', [url])
        : Platform.isWindows
            ? ('explorer', [url])
            : ('xdg-open', [url]);
    Process.run(cmd, args).ignore();
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

  Future<void> _editConnection(ProjectItem i) => showDialog<bool>(
      context: context,
      builder: (_) => EditConnectionDialog(state: state, item: i, env: state.selectedEnvSummary!));

  Future<void> _reveal(ProjectItem i) => _run(() => state.reveal(i.summary.folder));

  Future<void> _confirmRemove(ProjectItem i) async {
    final ok = await showDialog<bool>(context: context, builder: (_) => RemoveDialog(item: i));
    if (ok == true) await _run(() => state.removeOrDetach(i));
  }

  Future<void> _addEnv(ProjectItem i) =>
      showDialog<bool>(context: context, builder: (_) => AddEnvDialog(state: state, item: i));

  Future<void> _editEnv(ProjectItem i, EnvSummary e) => showDialog<bool>(
      context: context, builder: (_) => EditConnectionDialog(state: state, item: i, env: e));

  Future<void> _confirmRemoveEnv(ProjectItem i, EnvSummary e) async {
    final ok = await showDialog<bool>(context: context, builder: (_) => RemoveEnvDialog(item: i, env: e));
    if (ok == true) await _run(() => state.removeEnvEntry(i, e));
  }

  Future<void> _syncAll() async {
    // Declines recorded for this one call only (a fresh `_syncAll()` --
    // another click of "Sync all" -- asks again): `_confirmTwoWay` awaited
    // sequentially per env clears its in-flight-dialog entry as soon as
    // each dialog resolves, so without tracking declines here ourselves, a
    // "no" on a project's first env would still let its second env pop a
    // fresh dialog, and so on for every remaining env of that project.
    final declinedTwoWay = <String>{};
    for (final p in state.projects) {
      for (final e in p.summary.envs) {
        // Skip envs a watch owns (watching or mid-stop): a one-shot Sync
        // there would double-subscribe, the exact hazard Step 3b's header
        // and row gates exist to prevent. Sync the rest; don't refuse the
        // whole bulk action for one watched env.
        if (_syncBlocked(state, p.summary.folder, e.name)) continue;
        if (declinedTwoWay.contains(p.summary.folder)) continue;
        if (await _confirmTwoWay(context, state, p)) {
          state.syncEnvItem(p, e);
        } else {
          declinedTwoWay.add(p.summary.folder);
        }
      }
    }
  }

  Future<void> _about() async {
    final version = await rdcVersion();
    if (!mounted) return;
    showAboutDialog(
      context: context,
      applicationName: 'rdc',
      applicationVersion: 'v${version ?? '?'}  •  rdc core embedded',
      children: const [
        Text('Cross-platform desktop front-end for the rdc core '
            '(Flutter + flutter_rust_bridge).'),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: ListenableBuilder(
        listenable: state,
        builder: (context, _) {
          // One dialog at a time, oldest first. A second env blocking while
          // this is open waits its turn; its row still shows the watching
          // badge.
          final pending = state.promptQueue;
          if (pending.isNotEmpty && !_promptOpen) {
            _promptOpen = true;
            final p = pending.first;
            _openPromptKey = state.envKey(p.folder, p.env);
            _openPromptId = p.id;
            WidgetsBinding.instance.addPostFrameCallback((_) async {
              await showDialog<void>(
                context: context,
                barrierDismissible: false, // a cycle is blocked; there is no "later"
                builder: (_) => PromptDialog(
                  prompt: p,
                  logTail: (state.syncLog[state.envKey(p.folder, p.env)] ?? const <String>[])
                      .reversed
                      .take(40)
                      .toList()
                      .reversed
                      .toList(),
                  onAnswer: (k) {
                    // Clear first: `state.answer` calls notifyListeners
                    // synchronously, which rebuilds this ListenableBuilder
                    // (and could hit the "resolved externally" branch
                    // below) before the `pop()` on the next line ever runs.
                    // With these already null, that branch sees nothing to
                    // close and leaves the explicit pop below as the only
                    // one.
                    _openPromptKey = null;
                    _openPromptId = null;
                    state.answer(p, k);
                    Navigator.of(context).pop();
                  },
                ),
              );
              _promptOpen = false;
              _openPromptKey = null;
              _openPromptId = null;
            });
          } else if (_promptOpen &&
              _openPromptKey != null &&
              state.pendingPrompts[_openPromptKey]?.id != _openPromptId) {
            // The prompt the open dialog is showing resolved some other
            // way than the user pressing a button here — the watch was
            // stopped, the stream errored, or a `SyncPhase::Error` arrived
            // — so nothing else will close it. `PromptResolved`'s whole
            // documented purpose is "close the dialog"; this is that half.
            //
            // Cleared immediately (not just after the pop completes) so a
            // fresh prompt — for this env or another — can open on the very
            // next build instead of waiting for `showDialog`'s future,
            // which won't resolve until the frame below actually pops it.
            _openPromptKey = null;
            _openPromptId = null;
            WidgetsBinding.instance.addPostFrameCallback((_) {
              final nav = Navigator.of(context, rootNavigator: true);
              if (nav.canPop()) nav.pop();
            });
          }
          return MdhScaffold(
            state: state,
            view: _view,
            listWidth: _listWidth,
            activeTab: _tab,
            onResize: (dx) => setState(() => _listWidth = (_listWidth + dx).clamp(200.0, 460.0)),
            onSelectTab: (t) => setState(() => _tab = t),
            onSelectConn: (folder) => setState(() {
              state.selectProject(folder);
              _view = NavView.connection;
            }),
            onSelectEnv: (folder, env) => setState(() {
              state.selectEnv(folder, env);
              _view = NavView.connection;
            }),
            onSelectFleet: () => setState(() => _view = NavView.overview),
            onSelectSettings: () => setState(() => _view = NavView.settings),
            onAdd: _addConnection,
            onOpen: _openExisting,
            onSync: (p, e) async {
              if (await _confirmTwoWay(context, state, p)) state.syncEnvItem(p, e);
            },
            onSyncAll: _syncAll,
            onEdit: _editConnection,
            onReveal: _reveal,
            onRemove: _confirmRemove,
            onRevealDir: (path) => _run(() => state.reveal(path)),
            onAddEnv: _addEnv,
            onEditEnv: _editEnv,
            onRemoveEnv: _confirmRemoveEnv,
            onChooseParent: _chooseParent,
            onAbout: _about,
            onCheckUpdate: () async {
              final messenger = ScaffoldMessenger.of(context);
              final info = await checkForUpdate(await rdcVersion() ?? '');
              if (!mounted) return;
              messenger.showSnackBar(info == null
                  ? const SnackBar(
                      content: Text(
                          "You're on the latest version (or the check couldn't reach GitHub)."))
                  : _updateSnackBar(info));
            },
          );
        },
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
    this.onSelectEnv,
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
    this.onAddEnv,
    this.onRemoveEnv,
    this.onEditEnv,
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
  final void Function(String folder, String env)? onSelectEnv;
  final VoidCallback? onSelectFleet;
  final VoidCallback? onSelectSettings;
  final VoidCallback? onAdd;
  final VoidCallback? onOpen;
  final void Function(ProjectItem, EnvSummary)? onSync;
  final VoidCallback? onSyncAll;
  final void Function(ProjectItem)? onEdit;
  final void Function(ProjectItem)? onReveal;
  final void Function(ProjectItem)? onRemove;
  final void Function(String path)? onRevealDir;
  final void Function(ProjectItem)? onAddEnv;
  final void Function(ProjectItem, EnvSummary)? onRemoveEnv;
  final void Function(ProjectItem, EnvSummary)? onEditEnv;
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
      NavView.connection => state.selectedEnv == null
          ? _ProjectView(
              state: state,
              onSync: onSync ?? (_, _) {},
              onSelectEnv: onSelectEnv ?? (_, _) {},
              onAddEnv: onAddEnv ?? (_) {},
              onEditEnv: onEditEnv ?? (_, _) {},
              onRemoveEnv: onRemoveEnv ?? (_, _) {},
              onAdd: onAdd ?? () {},
            )
          : _ConnMain(
              state: state,
              activeTab: activeTab,
              onSelectTab: onSelectTab ?? (_) {},
              onSync: onSync ?? (_, _) {},
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
          onOpenConn: onSelectEnv ?? (_, _) {},
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
              onSelectEnv: onSelectEnv ?? (_, _) {},
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
    required this.onSelectEnv,
    required this.onSelectFleet,
    required this.onSelectSettings,
    required this.onAdd,
    required this.onOpen,
  });
  final AppState state;
  final NavView view;
  final void Function(String) onSelect;
  final void Function(String folder, String env) onSelectEnv;
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
                    mouseCursor: SystemMouseCursors.click,
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
                Text('PROJECTS',
                    style: TextStyle(color: c.textSecondary, fontSize: 11, fontWeight: FontWeight.w700, letterSpacing: 0.8)),
                const Spacer(),
                Tooltip(
                  message: 'New project',
                  child: InkWell(
                    onTap: onAdd,
                    mouseCursor: SystemMouseCursors.click,
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
            child: state.projects.isEmpty
                ? Center(
                    child: Padding(
                      padding: const EdgeInsets.all(16),
                      child: Text('No projects yet.\nAdd one with +.',
                          textAlign: TextAlign.center,
                          style: TextStyle(color: c.textSecondary, fontSize: 12)),
                    ),
                  )
                : ListView(
                    padding: const EdgeInsets.symmetric(horizontal: 8),
                    children: [
                      for (final p in state.projects) ...[
                        _ProjectRow(state: state, item: p, onSelect: onSelect),
                        for (final e in p.summary.envs)
                          _EnvRow(state: state, item: p, env: e, onSelect: onSelectEnv),
                      ],
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

class _ProjectRow extends StatelessWidget {
  const _ProjectRow({required this.state, required this.item, required this.onSelect});
  final AppState state;
  final ProjectItem item;
  final void Function(String) onSelect;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final sel = item.summary.folder == state.selectedFolder;
    return Padding(
      padding: const EdgeInsets.only(top: 4, bottom: 1),
      child: InkWell(
        onTap: () => onSelect(item.summary.folder),
        mouseCursor: SystemMouseCursors.click,
        borderRadius: BorderRadius.circular(6),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 6),
          child: Row(children: [
            Icon(Icons.folder_outlined, size: 15, color: sel ? c.accent : c.textSecondary),
            const SizedBox(width: 8),
            Expanded(child: Text(item.summary.name,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(color: c.textPrimary, fontSize: 13, fontWeight: FontWeight.w600))),
            if (item.isExternal) Text('ext', style: monoStyle(c.textHint, 10)),
          ]),
        ),
      ),
    );
  }
}

class _EnvRow extends StatelessWidget {
  const _EnvRow({required this.state, required this.item, required this.env, required this.onSelect});
  final AppState state;
  final ProjectItem item;
  final EnvSummary env;
  final void Function(String folder, String env) onSelect;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final sel = item.summary.folder == state.selectedFolder && env.name == state.selectedEnv;
    final st = _statusOf(state, item, env);
    final w = state.watch[state.envKey(item.summary.folder, env.name)];
    final sub = switch (st) {
      _St.running => 'syncing…',
      _St.watching => w?.nextPollSecs != null ? 'watching · ${w!.nextPollSecs}s' : 'watching',
      _St.error => 'failed',
      _St.synced => _rel(env.lastSyncUnix),
      _St.never => 'never',
    };
    final dotColor = switch (st) {
      _St.error => c.danger,
      _St.never => c.textHint,
      _St.watching => c.accent,
      _ => c.successFg,
    };
    return Padding(
      padding: const EdgeInsets.only(left: 14, top: 1, bottom: 1),
      child: InkWell(
        onTap: () => onSelect(item.summary.folder, env.name),
        mouseCursor: SystemMouseCursors.click,
        borderRadius: BorderRadius.circular(6),
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 6),
          decoration: BoxDecoration(
            color: sel ? c.accent : Colors.transparent, borderRadius: BorderRadius.circular(6)),
          child: Row(children: [
            Container(width: 7, height: 7, margin: const EdgeInsets.only(right: 10),
                decoration: BoxDecoration(color: sel ? Colors.white : dotColor, shape: BoxShape.circle)),
            Expanded(child: Text(env.name, overflow: TextOverflow.ellipsis,
                style: TextStyle(color: sel ? Colors.white : c.textPrimary, fontSize: 12.5, fontWeight: FontWeight.w500))),
            ConstrainedBox(
              // cap the trailing "org N · sub" text so a 6-digit org id can't overflow the row
              constraints: const BoxConstraints(maxWidth: 110),
              child: Text('org ${env.orgId} · $sub',
                  overflow: TextOverflow.ellipsis,
                  maxLines: 1,
                  style: monoStyle(sel ? Colors.white70 : c.textSecondary, 10.5)),
            ),
          ]),
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
  final void Function(ProjectItem, EnvSummary) onSync;
  final void Function(ProjectItem) onEdit, onReveal, onRemove;
  final void Function(String) onRevealDir;
  final VoidCallback onAdd;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final item = state.selected;
    final env = state.selectedEnvSummary;
    if (item == null || env == null) {
      return Center(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text('No environment selected', style: TextStyle(color: c.textSecondary)),
            const SizedBox(height: 12),
            MdhBtn(label: 'New project', primary: true, onTap: onAdd),
          ],
        ),
      );
    }
    final st = _statusOf(state, item, env);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _ConnBar(state: state, item: item, env: env, onSync: onSync, onEdit: onEdit, onReveal: onReveal, onRemove: onRemove),
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
                  key: ValueKey('files:${item.summary.folder}:${env.name}'),
                  rootFolder: item.summary.folder,
                  initialCrumbs: ['envs', env.name],
                  revision: env.fileCount.toInt(),
                  onRevealDir: onRevealDir,
                ),
              ),
            'log' => Padding(
                padding: const EdgeInsets.all(18),
                child: _SyncLogCard(state: state, item: item, env: env),
              ),
            _ => Padding(
                padding: const EdgeInsets.all(18),
                child: _OverviewPanel(state: state, item: item, env: env, st: st),
              ),
          },
        ),
      ],
    );
  }
}

class _ConnBar extends StatelessWidget {
  const _ConnBar({required this.state, required this.item, required this.env, required this.onSync, required this.onEdit, required this.onReveal, required this.onRemove});
  final AppState state;
  final ProjectItem item;
  final EnvSummary env;
  final void Function(ProjectItem, EnvSummary) onSync;
  final void Function(ProjectItem) onEdit, onReveal, onRemove;

  // The five action buttons' measured, unconstrained single-line width
  // (Sync 480.75px / Retry 493.5px — Retry is the wider label) rounded up
  // with a small margin, via a throwaway probe rendering exactly this
  // button row unconstrained. This is the row's true single-line floor:
  // below `_actionsIntrinsicWidth + spacer`, the buttons cannot coexist
  // with the title on one line no matter how far the title shrinks, so
  // there is nothing to tune here — re-measure if the buttons ever change.
  static const _actionsIntrinsicWidth = 500.0;

  // A sliver reserved for the title even in the tightest one-line case, so
  // it never shrinks to literally nothing before the layout switches to
  // wrapping the actions instead.
  static const _titleReserve = 40.0;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final st = _statusOf(state, item, env);
    final watching = state.isWatching(item.summary.folder, env.name);
    final stopping = _isStopping(state, item.summary.folder, env.name);
    final syncBlocked = _syncBlocked(state, item.summary.folder, env.name);

    final titleGroup = Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Flexible(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisSize: MainAxisSize.min,
            children: [
              Text('${item.summary.name} · ${env.name}', maxLines: 1, overflow: TextOverflow.ellipsis,
                  style: TextStyle(color: c.textPrimary, fontSize: 15, fontWeight: FontWeight.w600)),
              const SizedBox(height: 2),
              Text('${_host(env.apiBase)} · org ${env.orgId}', maxLines: 1, overflow: TextOverflow.ellipsis,
                  style: monoStyle(c.textSecondary, 12)),
            ],
          ),
        ),
        // Flexible even though it's a spacer: this whole Row can itself be
        // squeezed to almost nothing in the narrow/wrapping layout below
        // (Expanded(flex: 1) there can hand it well under 12px). A bare
        // SizedBox is rigid — Flex never shrinks a non-flex child below its
        // declared size — so at some point it alone would demand more
        // width than the Row has, throwing regardless of how far the
        // Column/pill around it can shrink. Wrapped in Flexible it clamps
        // down to whatever's actually left instead.
        const Flexible(child: SizedBox(width: 12)),
        // Flexible (not a rigid sibling) so the pill can also give up room
        // under extreme width pressure instead of forcing a hard RenderFlex
        // overflow; at any width with room to spare it just renders at its
        // natural size, identical to before.
        Flexible(child: _StatusPill(st: st)),
      ],
    );

    final syncBtn = Tooltip(
      message: syncBlocked ? 'Stop watching before syncing' : (st == _St.error ? 'Retry' : 'Sync'),
      child: MdhBtn(
        label: st == _St.error ? 'Retry' : 'Sync',
        primary: true,
        onTap: (st == _St.running || syncBlocked) ? null : () => onSync(item, env),
      ),
    );
    final watchBtn = Tooltip(
      message: watching
          ? 'Stop watching'
          : (stopping ? 'Stopping — wait for it to finish' : 'Watch this environment'),
      child: MdhBtn(
        label: watching ? 'Stop' : 'Watch',
        onTap: watching
            ? () => state.stopWatchItem(item, env)
            : (stopping
                ? null
                : () async {
                    // A watch's first action is a full two-way reconcile,
                    // so this needs the same gate Sync has -- otherwise
                    // starting a watch on a never-synced-by-this-build
                    // project would push without the notice ever showing.
                    if (await _confirmTwoWay(context, state, item, forWatch: true)) {
                      state.watchEnvItem(item, env);
                    }
                  }),
      ),
    );
    final editBtn = MdhBtn(label: 'Edit', onTap: () => onEdit(item));
    final revealBtn = MdhBtn(label: 'Reveal', onTap: () => onReveal(item));
    final removeBtn = MdhBtn(label: item.isExternal ? 'Detach' : 'Remove', onTap: () => onRemove(item));
    final actionWidgets = [syncBtn, watchBtn, editBtn, revealBtn, removeBtn];

    return Container(
      padding: const EdgeInsets.fromLTRB(18, 12, 18, 12),
      decoration: BoxDecoration(
        color: c.bgCard,
        border: Border(bottom: BorderSide(color: c.border)),
      ),
      // A fixed Expanded(actions) share (tried in an earlier pass) doesn't
      // track how much room the buttons actually need: any ratio generous
      // enough to hold one line at ordinary widths still forces a wrap at
      // ample ones (the buttons don't need 2/3 of a 1080px-wide bar), so
      // the header wrapped far earlier than the buttons' own natural width
      // required. LayoutBuilder makes that comparison for real: above the
      // buttons' measured single-line floor, this renders literally the
      // pre-Task-14 layout (Expanded title takes whatever the buttons'
      // *actual* width leaves, exactly like a plain, un-flexed button row
      // would) — intrinsic sizing, no tuned ratio. Only below that floor,
      // where no title width (down to _titleReserve) would make the
      // buttons fit on one line, does it switch to a bounded, wrapping
      // Wrap — degrading instead of throwing a RenderFlex overflow.
      child: LayoutBuilder(
        builder: (context, constraints) {
          final oneLineFits = constraints.maxWidth >= _actionsIntrinsicWidth + 12 + _titleReserve;
          if (oneLineFits) {
            return Row(
              children: [
                Expanded(child: titleGroup),
                const SizedBox(width: 12),
                syncBtn,
                const SizedBox(width: 8),
                watchBtn,
                const SizedBox(width: 8),
                editBtn,
                const SizedBox(width: 8),
                revealBtn,
                const SizedBox(width: 8),
                removeBtn,
              ],
            );
          }
          return Row(
            children: [
              Expanded(child: titleGroup),
              const SizedBox(width: 12),
              Expanded(
                flex: 2,
                child: Wrap(
                  alignment: WrapAlignment.end,
                  spacing: 8,
                  runSpacing: 8,
                  children: actionWidgets,
                ),
              ),
            ],
          );
        },
      ),
    );
  }
}

class _OverviewPanel extends StatelessWidget {
  const _OverviewPanel({required this.state, required this.item, required this.env, required this.st});
  final AppState state;
  final ProjectItem item;
  final EnvSummary env;
  final _St st;

  @override
  Widget build(BuildContext context) {
    // Connection details (host, org) already live in the header, so the panel
    // is just the at-a-glance stat cards plus a recent-sync log that fills the
    // remaining height.
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(children: [
          Expanded(child: _StatCard(n: env.fileCount.toString(), l: 'Files pulled')),
          const SizedBox(width: 12),
          Expanded(child: _StatCard(n: st == _St.never ? '—' : _rel(env.lastSyncUnix), l: 'Last sync')),
          const SizedBox(width: 12),
          Expanded(child: _StatCard(n: env.authKind == AuthKind.token ? 'token' : 'login', l: 'Auth')),
        ]),
        _SectionTitle('Recent sync'),
        Expanded(child: _SyncLogCard(state: state, item: item, env: env)),
      ],
    );
  }
}

/// The sync-log panel. Fills the height it's given (place it in an Expanded);
/// short logs sit at the top, long logs auto-scroll to the latest line.
class _SyncLogCard extends StatefulWidget {
  const _SyncLogCard({required this.state, required this.item, required this.env});
  final AppState state;
  final ProjectItem item;
  final EnvSummary env;
  @override
  State<_SyncLogCard> createState() => _SyncLogCardState();
}

class _SyncLogCardState extends State<_SyncLogCard> {
  final _scroll = ScrollController();
  bool _follow = true; // tail the log unless the user has scrolled up

  @override
  void initState() {
    super.initState();
    _scroll.addListener(_onScroll);
  }

  @override
  void dispose() {
    _scroll.removeListener(_onScroll);
    _scroll.dispose();
    super.dispose();
  }

  void _onScroll() {
    if (!_scroll.hasClients) return;
    final p = _scroll.position;
    _follow = p.maxScrollExtent - p.pixels <= 40;
  }

  // Keep the latest line in view as it streams, but don't yank the view down
  // when the user has scrolled up to read earlier output.
  void _tail() {
    if (!_follow || !_scroll.hasClients) return;
    final p = _scroll.position;
    if (p.pixels != p.maxScrollExtent) _scroll.jumpTo(p.maxScrollExtent);
  }

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final st = _statusOf(widget.state, widget.item, widget.env);
    final k = widget.state.envKey(widget.item.summary.folder, widget.env.name);
    final lines = widget.state.syncLog[k] ?? const <String>[];
    final msg = widget.state.syncMessage[k];

    Widget body;
    if (lines.isNotEmpty) {
      final spans = <InlineSpan>[];
      for (var k = 0; k < lines.length; k++) {
        spans.addAll(ansiSpans(lines[k], c, 12.5));
        if (k < lines.length - 1) spans.add(const TextSpan(text: '\n'));
      }
      // Follow the tail as new lines stream in (no-op when it all fits).
      WidgetsBinding.instance.addPostFrameCallback((_) => _tail());
      body = Scrollbar(
        controller: _scroll,
        child: SingleChildScrollView(
          controller: _scroll,
          child: SizedBox(width: double.infinity, child: SelectableText.rich(TextSpan(children: spans))),
        ),
      );
    } else {
      final (String text, Color col) = switch (st) {
        _St.running => ('syncing…', c.textPrimary),
        _St.watching => ('watching…', c.textPrimary),
        _St.error => ('✕ ${msg ?? 'sync failed'}', c.dangerFg),
        _St.synced => ('✓ ${msg ?? 'up to date · ${_rel(widget.env.lastSyncUnix)} ago'}', c.successFg),
        _St.never => ('— not synced yet', c.textSecondary),
      };
      body = Align(alignment: Alignment.topLeft, child: SelectableText(text, style: monoStyle(col, 12.5)));
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

// ------------------------------------------------------------ project pane

/// Shown when a project node (not one of its envs) is selected
/// (`state.selectedEnv == null`): the Environments table for that project,
/// with per-env Sync/Edit/Remove and an "Add environment" action.
class _ProjectView extends StatelessWidget {
  const _ProjectView({
    required this.state,
    required this.onSync,
    required this.onSelectEnv,
    required this.onAddEnv,
    required this.onEditEnv,
    required this.onRemoveEnv,
    required this.onAdd,
  });
  final AppState state;
  final void Function(ProjectItem, EnvSummary) onSync;
  final void Function(String folder, String env) onSelectEnv;
  final void Function(ProjectItem) onAddEnv;
  final void Function(ProjectItem, EnvSummary) onEditEnv;
  final void Function(ProjectItem, EnvSummary) onRemoveEnv;
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
            Text('No projects yet', style: TextStyle(color: c.textSecondary)),
            const SizedBox(height: 12),
            MdhBtn(label: 'New project', primary: true, onTap: onAdd),
          ],
        ),
      );
    }
    final envs = item.summary.envs;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _ProjectBar(
          item: item,
          onSyncAll: envs.isEmpty
              ? null
              : () {
                  for (final e in envs) {
                    // Same skip as the Fleet view's "Sync all": don't
                    // double-subscribe an env a watch already owns.
                    if (_syncBlocked(state, item.summary.folder, e.name)) continue;
                    onSync(item, e);
                  }
                },
          onAddEnv: () => onAddEnv(item),
        ),
        Expanded(
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(18),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                _SectionTitle('Environments'),
                _EnvTable(
                  state: state,
                  item: item,
                  envs: envs,
                  onSync: onSync,
                  onEdit: onEditEnv,
                  onRemove: onRemoveEnv,
                  onSelectEnv: onSelectEnv,
                ),
              ],
            ),
          ),
        ),
      ],
    );
  }
}

class _ProjectBar extends StatelessWidget {
  const _ProjectBar({required this.item, required this.onSyncAll, required this.onAddEnv});
  final ProjectItem item;
  final VoidCallback? onSyncAll;
  final VoidCallback onAddEnv;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Container(
      padding: const EdgeInsets.fromLTRB(18, 12, 18, 12),
      decoration: BoxDecoration(
        color: c.bgCard,
        border: Border(bottom: BorderSide(color: c.border)),
      ),
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: MainAxisSize.min,
              children: [
                Text(item.summary.name, maxLines: 1, overflow: TextOverflow.ellipsis,
                    style: TextStyle(color: c.textPrimary, fontSize: 15, fontWeight: FontWeight.w600)),
                const SizedBox(height: 2),
                Text(item.summary.folder, maxLines: 1, overflow: TextOverflow.ellipsis,
                    style: monoStyle(c.textSecondary, 12)),
              ],
            ),
          ),
          const SizedBox(width: 12),
          MdhBtn(label: 'Sync all envs', primary: true, onTap: onSyncAll),
          const SizedBox(width: 8),
          MdhBtn(label: 'Add environment', onTap: onAddEnv),
        ],
      ),
    );
  }
}

// ---------------------------------------------------- table columns

/// How wide one column of an environment table wants to be.
///
/// `w` is what it takes when the table has room; `flex` is the share it
/// falls back to when it does not. Environment/Connection and Host have no
/// `w` — they hold arbitrary-length strings (a project name plus env name, a
/// hostname), ellipsize, and always flex, absorbing whatever the bounded
/// columns leave.
class _ColSpec {
  const _ColSpec({this.w, required this.flex});
  final double? w;
  final int flex;
}

/// Shared column geometry for the two environment tables — `_EnvTable` in
/// the Project view and `_FleetTable` in the fleet overview — so a header
/// and its rows cannot drift apart, and so the two tables agree.
///
/// The four bounded columns get a FIXED width whenever the table is at least
/// `minTight` wide, because their content has a known maximum: an org id, a
/// file count, `_rel`'s output, and the widest status word. Below that the
/// table falls back to sharing every column by flex, which is how it behaved
/// before — nothing goes off-screen and nothing overflows, it just gets
/// tight, and `maxLines: 1` means it ellipsizes rather than restacking.
///
/// Before this, the columns *only* shared a flex pool, and the action
/// cluster took 4 of the 12 — handing four 26px icon buttons a third of the
/// whole table (a measured 267.7px at the default 1180px window, 354.3px at
/// 1440px) while six info columns split what was left. The status badge got
/// 29.9px of it and wrapped `synced` onto three lines; `…d ago` also took
/// three, the org id two, and four of the seven headers two. `_MiniBadge`
/// additionally had no `maxLines`/`softWrap` guard at all, unlike its
/// sibling `_StatusPill`.
///
/// Each `w` is max(header label, widest data) + 28 cell padding, with ~8px
/// of margin. The numbers in the comments are TextPainter measurements taken
/// in the real font from the running app (a temporary probe in `main()`, see
/// the commit message) rather than estimates — three of these columns are
/// sized by their *header*, not their data, and sizing to the data alone
/// left `LAST SYNC` needing 99px in a 64px column.
///
/// Deliberately NOT sized for the font `flutter test` substitutes, whose
/// glyphs are ~1.0248em wide against this font's ~0.55em. Sizing for that
/// one would have cost ~124px of real table width, taken straight out of
/// Environment and Host, to make a test convenient. The consequence is that
/// these cells do truncate under the test font, so
/// `env_table_layout_test.dart` asserts only font-independent properties and
/// absolute fit rests on the measurements here.
class _Cols {
  static const env = _ColSpec(flex: 3);
  static const host = _ColSpec(flex: 2);
  static const org = _ColSpec(w: 86, flex: 1); // ORG 24.8 / 7-digit id 50.6
  static const files = _ColSpec(w: 72, flex: 1); // FILES 31.4 / 5 digits 36.1
  static const lastSync = _ColSpec(w: 100, flex: 1); // LAST SYNC 64.4 / '9999d' 39.5
  static const status = _ColSpec(w: 100, flex: 2); // STATUS 44.3 / 'watching' badge 63.2
  /// Four 26px icon buttons + three 6px gaps = 122, + 28 cell padding. Its
  /// flex fallback is 3 rather than the 4 it used to have unconditionally.
  static const actions = _ColSpec(w: 150, flex: 3);

  static const _fixed = 86.0 + 72 + 100 + 100; // the four bounded columns
  /// Floor for the flex columns, below which fixed widths stop paying for
  /// themselves and the all-flex fallback is kinder.
  static const _flexFloor = 120.0;

  /// At or above this content width a table uses fixed widths for its
  /// bounded columns; below it, everything shares by flex. Two values
  /// because only `_EnvTable` carries an action cluster.
  static const minTightEnv = _fixed + 150 + _flexFloor;
  static const minTightFleet = _fixed + _flexFloor;
}

/// One table cell. Exactly one of `width` (a bounded column) or `flex` (a
/// column that absorbs the leftover) must be given.
Widget _tCell(Widget child, {int? flex, double? width, Key? key}) {
  assert((flex == null) != (width == null), 'give exactly one of flex/width');
  final padded = Padding(
    padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 11),
    child: child,
  );
  return width != null
      ? SizedBox(key: key, width: width, child: padded)
      : Expanded(key: key, flex: flex!, child: padded);
}

/// One cell of a column, fixed-width or flexed depending on the regime the
/// table picked. A spec with no `w` always flexes.
Widget _colCell(Widget child, _ColSpec s, bool tight, {Key? key}) =>
    (tight && s.w != null) ? _tCell(child, width: s.w, key: key) : _tCell(child, flex: s.flex, key: key);

/// A column header. Single-line by construction: a label that outgrows its
/// column clips instead of silently restacking every row in the table.
Widget _tHead(MdhColors c, String t, _ColSpec s, bool tight) => _colCell(
      Text(t.toUpperCase(),
          maxLines: 1,
          softWrap: false,
          overflow: TextOverflow.ellipsis,
          style: TextStyle(color: c.textSecondary, fontSize: 10.5, fontWeight: FontWeight.w600, letterSpacing: 0.5)),
      s,
      tight,
    );


class _EnvTable extends StatelessWidget {
  const _EnvTable({
    required this.state,
    required this.item,
    required this.envs,
    required this.onSync,
    required this.onEdit,
    required this.onRemove,
    required this.onSelectEnv,
  });
  final AppState state;
  final ProjectItem item;
  final List<EnvSummary> envs;
  final void Function(ProjectItem, EnvSummary) onSync, onEdit, onRemove;
  final void Function(String folder, String env) onSelectEnv;

  // Column widths live in _Cols, shared with _FleetTable. The info columns
  // still nest inside one Expanded (see _EnvTableRow) to keep the action
  // buttons outside the row's tap target; the header mirrors that nesting so
  // the two line up.

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    if (envs.isEmpty) {
      return Container(
        width: double.infinity,
        padding: const EdgeInsets.all(24),
        decoration: BoxDecoration(color: c.bgCard, border: Border.all(color: c.borderCard), borderRadius: BorderRadius.circular(6)),
        child: Center(child: Text('No environments yet.', style: TextStyle(color: c.textSecondary))),
      );
    }

    return Container(
      decoration: BoxDecoration(color: c.bgCard, border: Border.all(color: c.borderCard), borderRadius: BorderRadius.circular(6),
          boxShadow: [BoxShadow(color: Colors.black.withValues(alpha: 0.04), blurRadius: 3, offset: const Offset(0, 1))]),
      clipBehavior: Clip.antiAlias,
      // One LayoutBuilder for the whole table, not one per cell: the header
      // and every row must agree on the regime or their columns won't line
      // up.
      child: LayoutBuilder(builder: (_, cs) {
        final tight = cs.maxWidth >= _Cols.minTightEnv;
        return Column(children: [
          Container(
            decoration: BoxDecoration(color: c.bgSidebar, border: Border(bottom: BorderSide(color: c.border))),
            child: Row(children: [
              Expanded(
                child: Row(children: [
                  _tHead(c, 'Environment', _Cols.env, tight),
                  _tHead(c, 'Org', _Cols.org, tight),
                  _tHead(c, 'Host', _Cols.host, tight),
                  _tHead(c, 'Files', _Cols.files, tight),
                  _tHead(c, 'Last sync', _Cols.lastSync, tight),
                  _tHead(c, 'Status', _Cols.status, tight),
                ]),
              ),
              _tHead(c, 'Actions', _Cols.actions, tight),
            ]),
          ),
          for (var i = 0; i < envs.length; i++)
            _EnvTableRow(
              state: state, item: item, env: envs[i], last: i == envs.length - 1, tight: tight,
              onSync: onSync, onEdit: onEdit, onRemove: onRemove, onSelectEnv: onSelectEnv,
            ),
        ]);
      }),
    );
  }
}

class _EnvTableRow extends StatelessWidget {
  const _EnvTableRow({
    required this.state,
    required this.item,
    required this.env,
    required this.last,
    required this.tight,
    required this.onSync,
    required this.onEdit,
    required this.onRemove,
    required this.onSelectEnv,
  });
  final AppState state;
  final ProjectItem item;
  final EnvSummary env;
  final bool last;
  /// Whether the table chose fixed widths; see `_Cols`.
  final bool tight;
  final void Function(ProjectItem, EnvSummary) onSync, onEdit, onRemove;
  final void Function(String folder, String env) onSelectEnv;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final st = _statusOf(state, item, env);
    final (String badge, Color bg, Color fg) = _badgeFor(c, st);
    final watching = state.isWatching(item.summary.folder, env.name);
    final stopping = _isStopping(state, item.summary.folder, env.name);
    final syncBlocked = _syncBlocked(state, item.summary.folder, env.name);
    Widget actionsBox(Widget child) => tight
        ? SizedBox(width: _Cols.actions.w, child: child)
        : Expanded(flex: _Cols.actions.flex, child: child);
    return Container(
      decoration: BoxDecoration(border: last ? null : Border(bottom: BorderSide(color: c.border))),
      child: Row(children: [
        // Info cells are the tap target for row selection; the action
        // cluster below sits outside this InkWell so its buttons don't also
        // trigger onSelectEnv.
        Expanded(
          child: InkWell(
            onTap: () => onSelectEnv(item.summary.folder, env.name),
            mouseCursor: SystemMouseCursors.click,
            child: Row(children: [
              _colCell(Text('${item.summary.name} · ${env.name}', maxLines: 1, overflow: TextOverflow.ellipsis,
                  style: TextStyle(color: c.textPrimary, fontSize: 12.5, fontWeight: FontWeight.w600)), _Cols.env, tight),
              _colCell(Text(env.orgId.toString(), maxLines: 1, overflow: TextOverflow.ellipsis,
                  style: monoStyle(c.textSecondary, 12)), _Cols.org, tight),
              _colCell(Text(_host(env.apiBase), maxLines: 1, overflow: TextOverflow.ellipsis,
                  style: monoStyle(c.textSecondary, 12)), _Cols.host, tight),
              _colCell(Text(env.fileCount.toString(), maxLines: 1, overflow: TextOverflow.ellipsis,
                  style: monoStyle(c.textSecondary, 12)), _Cols.files, tight),
              // No ' ago' suffix: the header already says LAST SYNC, and
              // dropping it is what keeps '999d' inside a column narrow
              // enough to leave the two flex columns real width.
              _colCell(Text(st == _St.never ? '—' : _rel(env.lastSyncUnix), maxLines: 1, overflow: TextOverflow.ellipsis,
                  style: TextStyle(color: c.textPrimary, fontSize: 12.5)), _Cols.lastSync, tight),
              // Keyed so env_table_layout_test.dart can assert the badge
              // actually fits the column, which a Text-level check cannot
              // see: the badge sets softWrap: false, so it lays out at its
              // intrinsic width and any clipping happens at this cell.
              _colCell(Align(alignment: Alignment.centerLeft, child: _MiniBadge(badge, bg, fg)),
                  _Cols.status, tight, key: ValueKey('status-cell-${env.name}')),
            ]),
          ),
        ),
        // The action cluster builds its own box rather than going through
        // _colCell, because it keeps its own vertical padding (8, not the
        // cell default's 11) to sit four 26px buttons centred in the row.
        //
        // Icon buttons (not MdhBtn) — narrow by design so more table columns
        // fit, unlike the header bar's full-label buttons. In the tight
        // regime _Cols.actions.w reserves exactly their intrinsic width; in
        // the flex fallback nothing guarantees it, which is why this is a
        // Wrap and not a Row: the icons flow onto a second line instead of
        // throwing a RenderFlex overflow.
        actionsBox(Padding(
            padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
            child: Wrap(alignment: WrapAlignment.end, spacing: 6, runSpacing: 4, children: [
              _RowIconBtn(
                icon: Icons.sync,
                tooltip: syncBlocked ? 'Stop watching before syncing' : (st == _St.error ? 'Retry' : 'Sync'),
                onTap: (st == _St.running || syncBlocked) ? null : () => onSync(item, env),
              ),
              _RowIconBtn(
                icon: watching ? Icons.visibility : Icons.visibility_outlined,
                tooltip: watching ? 'Stop watching' : (stopping ? 'Stopping…' : 'Watch'),
                onTap: watching
                    ? () => state.stopWatchItem(item, env)
                    : (stopping
                        ? null
                        : () async {
                            // Same two-way gate as _ConnBar's Watch button.
                            if (await _confirmTwoWay(context, state, item, forWatch: true)) {
                              state.watchEnvItem(item, env);
                            }
                          }),
              ),
              _RowIconBtn(icon: Icons.edit_outlined, tooltip: 'Edit', onTap: () => onEdit(item, env)),
              _RowIconBtn(icon: Icons.delete_outline, tooltip: 'Remove', danger: true, onTap: () => onRemove(item, env)),
            ]))),
      ]),
    );
  }
}

/// Compact icon-only action button for a table row, where a full labeled
/// `_Btn` cluster wouldn't fit. `tooltip` doubles as its accessible/test label.
class _RowIconBtn extends StatelessWidget {
  const _RowIconBtn({required this.icon, required this.tooltip, this.onTap, this.danger = false});
  final IconData icon;
  final String tooltip;
  final VoidCallback? onTap;
  final bool danger;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    return Tooltip(
      message: tooltip,
      child: Opacity(
        opacity: onTap == null ? 0.4 : 1,
        child: InkWell(
          onTap: onTap,
          mouseCursor: onTap == null ? SystemMouseCursors.basic : SystemMouseCursors.click,
          borderRadius: BorderRadius.circular(6),
          child: Container(
            width: 26,
            height: 26,
            decoration: BoxDecoration(color: c.bgCard, border: Border.all(color: c.border), borderRadius: BorderRadius.circular(6)),
            child: Icon(icon, size: 14, color: danger ? c.danger : c.textSecondary),
          ),
        ),
      ),
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
  const _FilesPanel({super.key, required this.rootFolder, required this.initialCrumbs, required this.revision, required this.onRevealDir});
  final String rootFolder;
  final List<String> initialCrumbs; // sub-path to open at; parents above it stay navigable
  final int revision; // reload trigger (grows after a sync)
  final void Function(String absPath) onRevealDir;
  @override
  State<_FilesPanel> createState() => _FilesPanelState();
}

class _FilesPanelState extends State<_FilesPanel> {
  List<String> _crumbs = [];
  List<_FEntry> _entries = const [];
  String? _error;
  bool _notSynced = false; // envs/<env>/ doesn't exist yet (never synced)

  // Read-only file preview. Non-null [_previewName] → previewing that file in
  // the current directory; [_previewText] holds its content, or [_previewNote]
  // explains why it can't be shown (binary / too large / unreadable).
  String? _previewName;
  String? _previewText;
  String? _previewNote;
  final _preview = ScrollController();

  static const _maxPreviewBytes = 2 * 1024 * 1024;

  String get _sep => Platform.pathSeparator;
  String get _dirPath => [widget.rootFolder, ..._crumbs].join(_sep);

  @override
  void initState() {
    super.initState();
    _crumbs = List.of(widget.initialCrumbs);
    _readInto();
  }

  @override
  void didUpdateWidget(covariant _FilesPanel old) {
    super.didUpdateWidget(old);
    // Env switches change the ValueKey (env name) → fresh State + initState, so
    // here we only need to handle a project (rootFolder) change; either way we
    // re-open at initialCrumbs.
    if (old.rootFolder != widget.rootFolder) {
      _crumbs = List.of(widget.initialCrumbs);
      _clearPreview();
      _readInto();
    } else if (old.revision != widget.revision) {
      _readInto();
      if (_previewName != null) _loadPreview(_previewName!);
    }
  }

  @override
  void dispose() {
    _preview.dispose();
    super.dispose();
  }

  bool _hidden(String name) => name.startsWith('.');

  void _readInto() {
    if (!Directory(_dirPath).existsSync()) {
      _entries = const [];
      _error = null;
      _notSynced = true;
      return;
    }
    _notSynced = false;
    try {
      final list = Directory(_dirPath).listSync(followLinks: false);
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

  void _loadPreview(String name) {
    _previewName = name;
    _previewText = null;
    _previewNote = null;
    try {
      final f = File('$_dirPath$_sep$name');
      final len = f.lengthSync();
      if (len > _maxPreviewBytes) {
        _previewNote = 'File is too large to preview (${_fmtSize(len)}).';
        return;
      }
      final bytes = f.readAsBytesSync();
      if (bytes.contains(0)) {
        _previewNote = 'Binary file — preview not available.';
        return;
      }
      try {
        _previewText = utf8.decode(bytes);
      } on FormatException {
        _previewNote = 'Binary file — preview not available.';
      }
    } catch (e) {
      _previewNote = errorText(e);
    }
  }

  void _clearPreview() {
    _previewName = null;
    _previewText = null;
    _previewNote = null;
  }

  void _goDir(List<String> crumbs) => setState(() {
        _crumbs = crumbs;
        _clearPreview();
        _readInto();
      });

  void _openFile(String name) => setState(() {
        _loadPreview(name);
        if (_preview.hasClients) _preview.jumpTo(0);
      });

  void _back() {
    if (_previewName != null) {
      setState(_clearPreview);
    } else if (_crumbs.isNotEmpty) {
      _goDir(_crumbs.sublist(0, _crumbs.length - 1));
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final previewing = _previewName != null;
    final rootName = widget.rootFolder.split(_sep).last;
    final dirSegs = [rootName, ..._crumbs]; // navigable directory segments

    // Breadcrumb: navigable directory segments, then the file name when previewing.
    final crumbs = <Widget>[];
    void sep() => crumbs.add(Padding(
        padding: const EdgeInsets.symmetric(horizontal: 3), child: Text('›', style: monoStyle(c.textHint, 12.5))));
    for (var i = 0; i < dirSegs.length; i++) {
      final current = i == dirSegs.length - 1 && !previewing;
      crumbs.add(InkWell(
        onTap: current ? null : () => _goDir(_crumbs.sublist(0, i)),
        mouseCursor: current ? SystemMouseCursors.basic : SystemMouseCursors.click,
        borderRadius: BorderRadius.circular(4),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 2, vertical: 2),
          child: Text(dirSegs[i], style: monoStyle(current ? c.textPrimary : c.textSecondary, 12.5, current ? FontWeight.w600 : FontWeight.w400)),
        ),
      ));
      if (i < dirSegs.length - 1 || previewing) sep();
    }
    if (previewing) {
      crumbs.add(Padding(
        padding: const EdgeInsets.symmetric(horizontal: 2, vertical: 2),
        child: Text(_previewName!, style: monoStyle(c.textPrimary, 12.5, FontWeight.w600)),
      ));
    }

    final canBack = previewing || _crumbs.isNotEmpty;
    final revealTarget = previewing ? '$_dirPath$_sep$_previewName' : _dirPath;

    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Row(
          children: [
            Opacity(
              opacity: canBack ? 1 : 0.45,
              child: InkWell(
                onTap: canBack ? _back : null,
                mouseCursor: canBack ? SystemMouseCursors.click : SystemMouseCursors.basic,
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
            MdhBtn(label: 'Reveal in Finder', onTap: () => widget.onRevealDir(revealTarget)),
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
            child: previewing ? _previewView(c) : _listView(c),
          ),
        ),
      ],
    );
  }

  Widget _listView(MdhColors c) {
    return Column(
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
    );
  }

  Widget _list(MdhColors c) {
    if (_notSynced) {
      return Center(
        child: Text("This environment hasn't been synced yet.", style: TextStyle(color: c.textSecondary, fontSize: 12.5)),
      );
    }
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
          onTap: e.isDir ? () => _goDir([..._crumbs, e.name]) : () => _openFile(e.name),
          mouseCursor: SystemMouseCursors.click,
          child: Container(
            decoration: BoxDecoration(border: last ? null : Border(bottom: BorderSide(color: c.border))),
            padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 9),
            child: Row(children: [
              Icon(e.isDir ? Icons.folder_rounded : Icons.insert_drive_file_outlined,
                  size: 18, color: e.isDir ? c.accent : c.textSecondary),
              const SizedBox(width: 12),
              Expanded(child: Text(e.name, overflow: TextOverflow.ellipsis, style: monoStyle(c.textPrimary, 13, FontWeight.w500))),
              const SizedBox(width: 12),
              Text(e.isDir ? '${e.count} item${e.count == 1 ? '' : 's'}' : _fmtSize(e.size), style: monoStyle(c.textHint, 12)),
            ]),
          ),
        );
      },
    );
  }

  Widget _previewView(MdhColors c) {
    if (_previewNote != null) {
      return Center(
        child: Padding(
          padding: const EdgeInsets.all(24),
          child: SelectableText(_previewNote!, textAlign: TextAlign.center, style: TextStyle(color: c.textSecondary, fontSize: 12.5)),
        ),
      );
    }
    final text = _previewText ?? '';
    if (text.isEmpty) {
      return Center(child: Text('Empty file.', style: TextStyle(color: c.textSecondary, fontSize: 12.5)));
    }
    // Vertical scroll (with a visible scrollbar) over a horizontal scroll so
    // long code/JSON lines don't wrap — read-only, selectable, basic syntax
    // highlighting by file type.
    return Scrollbar(
      controller: _preview,
      child: SingleChildScrollView(
        controller: _preview,
        padding: const EdgeInsets.all(14),
        child: SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          child: SelectableText.rich(
            TextSpan(children: highlightSource(text, _ext(_previewName), c, 12.5)),
          ),
        ),
      ),
    );
  }

  /// Lower-case extension of [name] without the dot, or null (also for dotfiles).
  String? _ext(String? name) {
    if (name == null) return null;
    final i = name.lastIndexOf('.');
    return (i <= 0 || i == name.length - 1) ? null : name.substring(i + 1).toLowerCase();
  }
}

// ------------------------------------------------------------ fleet overview

class _FleetView extends StatelessWidget {
  const _FleetView({required this.state, required this.onNew, required this.onSyncAll, required this.onOpenConn});
  final AppState state;
  final VoidCallback onNew, onSyncAll;
  final void Function(String folder, String env) onOpenConn;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final rows = [for (final p in state.projects) for (final e in p.summary.envs) (p, e)];
    final now = DateTime.now();
    var syncedToday = 0, files = 0, errors = 0;
    for (final (p, e) in rows) {
      final u = e.lastSyncUnix;
      if (u != null && now.difference(DateTime.fromMillisecondsSinceEpoch(u * 1000)).inHours < 24) {
        syncedToday++;
      }
      files += e.fileCount.toInt();
      if (state.syncState[state.envKey(p.summary.folder, e.name)] == SyncState.error) errors++;
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
              Text(state.parentFolder ?? '', style: monoStyle(c.textSecondary, 12)),
            ]),
            const Spacer(),
            MdhBtn(label: 'Sync all', primary: true, onTap: rows.isEmpty ? null : onSyncAll),
            const SizedBox(width: 8),
            MdhBtn(label: 'New project', onTap: onNew),
          ]),
        ),
        Expanded(
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(18),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(children: [
                  Expanded(child: _StatCard(n: '${rows.length}', l: 'Environments')),
                  const SizedBox(width: 12),
                  Expanded(child: _StatCard(n: '$syncedToday', l: 'Synced today')),
                  const SizedBox(width: 12),
                  Expanded(child: _StatCard(n: '$errors', l: 'Needs attention', danger: errors > 0)),
                  const SizedBox(width: 12),
                  Expanded(child: _StatCard(n: '$files', l: 'Files pulled')),
                ]),
                _SectionTitle('Environments'),
                _FleetTable(state: state, rows: rows, onOpenConn: onOpenConn),
              ],
            ),
          ),
        ),
      ],
    );
  }
}

class _FleetTable extends StatelessWidget {
  const _FleetTable({required this.state, required this.rows, required this.onOpenConn});
  final AppState state;
  final List<(ProjectItem, EnvSummary)> rows;
  final void Function(String folder, String env) onOpenConn;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    if (rows.isEmpty) {
      return Container(
        width: double.infinity,
        padding: const EdgeInsets.all(24),
        decoration: BoxDecoration(color: c.bgCard, border: Border.all(color: c.borderCard), borderRadius: BorderRadius.circular(6)),
        child: Center(child: Text('No environments yet.', style: TextStyle(color: c.textSecondary))),
      );
    }

    return Container(
      decoration: BoxDecoration(color: c.bgCard, border: Border.all(color: c.borderCard), borderRadius: BorderRadius.circular(6),
          boxShadow: [BoxShadow(color: Colors.black.withValues(alpha: 0.04), blurRadius: 3, offset: const Offset(0, 1))]),
      clipBehavior: Clip.antiAlias,
      // Its own threshold, not _EnvTable's: this table has no action
      // cluster, so it reaches the tight regime on a narrower window.
      child: LayoutBuilder(builder: (_, cs) {
        final tight = cs.maxWidth >= _Cols.minTightFleet;
        return Column(children: [
          Container(
            decoration: BoxDecoration(color: c.bgSidebar, border: Border(bottom: BorderSide(color: c.border))),
            child: Row(children: [
              _tHead(c, 'Connection', _Cols.env, tight),
              _tHead(c, 'Org', _Cols.org, tight),
              _tHead(c, 'Host', _Cols.host, tight),
              _tHead(c, 'Files', _Cols.files, tight),
              _tHead(c, 'Last sync', _Cols.lastSync, tight),
              _tHead(c, 'Status', _Cols.status, tight),
            ]),
          ),
          for (var i = 0; i < rows.length; i++)
            _FleetRow(state: state, item: rows[i].$1, env: rows[i].$2, last: i == rows.length - 1,
                tight: tight, onOpenConn: onOpenConn),
        ]);
      }),
    );
  }
}

class _FleetRow extends StatelessWidget {
  const _FleetRow({required this.state, required this.item, required this.env, required this.last,
      required this.tight, required this.onOpenConn});
  final AppState state;
  final ProjectItem item;
  final EnvSummary env;
  final bool last;
  /// Whether the table chose fixed widths; see `_Cols`.
  final bool tight;
  final void Function(String folder, String env) onOpenConn;

  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final st = _statusOf(state, item, env);
    final (String badge, Color bg, Color fg) = _badgeFor(c, st);
    return InkWell(
      onTap: () => onOpenConn(item.summary.folder, env.name),
      mouseCursor: SystemMouseCursors.click,
      child: Container(
        decoration: BoxDecoration(border: last ? null : Border(bottom: BorderSide(color: c.border))),
        child: Row(children: [
          _colCell(Row(children: [
            Flexible(child: Text('${item.summary.name} · ${env.name}', maxLines: 1, overflow: TextOverflow.ellipsis, style: TextStyle(color: c.textPrimary, fontSize: 12.5))),
            if (item.isExternal) Padding(padding: const EdgeInsets.only(left: 6), child: _MiniBadge('external', c.extBg, c.extFg)),
          ]), _Cols.env, tight),
          _colCell(Text(env.orgId.toString(), maxLines: 1, overflow: TextOverflow.ellipsis,
              style: monoStyle(c.textSecondary, 12)), _Cols.org, tight),
          _colCell(Text(_host(env.apiBase), maxLines: 1, overflow: TextOverflow.ellipsis,
              style: monoStyle(c.textSecondary, 12)), _Cols.host, tight),
          _colCell(Text(env.fileCount.toString(), maxLines: 1, overflow: TextOverflow.ellipsis,
              style: monoStyle(c.textSecondary, 12)), _Cols.files, tight),
          _colCell(Text(st == _St.never ? '—' : _rel(env.lastSyncUnix), maxLines: 1, overflow: TextOverflow.ellipsis,
              style: TextStyle(color: c.textPrimary, fontSize: 12.5)), _Cols.lastSync, tight),
          _colCell(Align(alignment: Alignment.centerLeft, child: _MiniBadge(badge, bg, fg)),
              _Cols.status, tight, key: ValueKey('fleet-status-cell-${env.name}')),
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
              _SpecCard(k: 'Projects folder', v: state.parentFolder ?? '(not set)', width: 260),
              const SizedBox(height: 14),
              Wrap(spacing: 12, runSpacing: 12, children: [
                MdhBtn(label: 'Change folder…', onTap: onChooseParent),
                MdhBtn(label: 'Open existing project…', onTap: onOpen),
                MdhBtn(label: 'Check for updates', onTap: onCheckUpdate),
                MdhBtn(label: 'About rdc', onTap: onAbout),
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
          MdhBtn(label: 'Choose folder…', primary: true, onTap: onChoose),
        ]),
      ),
    );
  }
}

// ------------------------------------------------------------ shared bits

class _StatusPill extends StatelessWidget {
  const _StatusPill({required this.st});
  final _St st;
  @override
  Widget build(BuildContext context) {
    final c = MdhColors.of(context);
    final (String label, Color bg, Color fg, Color bd) = switch (st) {
      _St.running => ('● syncing', c.infoBg, c.infoFg, c.infoBorder),
      _St.watching => ('◐ watching', c.infoBg, c.accent, c.infoBorder),
      _St.error => ('✕ sync failed', c.dangerBg, c.dangerFg, c.dangerBorder),
      _St.synced => ('● synced', c.successBg, c.successFg, c.successBorder),
      _St.never => ('○ never synced', c.warningBg, c.warningFg, c.warningBorder),
    };
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 9, vertical: 4),
      decoration: BoxDecoration(color: bg, border: Border.all(color: bd), borderRadius: BorderRadius.circular(999)),
      child: Text(
        label,
        overflow: TextOverflow.ellipsis,
        maxLines: 1,
        softWrap: false,
        style: TextStyle(color: fg, fontSize: 11, fontWeight: FontWeight.w600),
      ),
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
            mouseCursor: SystemMouseCursors.click,
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
  const _SpecCard({required this.k, required this.v, this.width});
  final String k, v;

  /// Fixed width for standalone use; null lets the card fill its parent.
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
        SelectableText(v, style: monoStyle(c.textPrimary, 13)),
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
      mouseCursor: SystemMouseCursors.click,
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
      // A badge is a fixed token, so it must never wrap — its sibling
      // _StatusPill has carried this guard all along, and _MiniBadge not
      // having it is what wrapped 'synced' onto three lines. Clip rather
      // than ellipsize: a badge reading 'syn…' is worse than a clipped one,
      // and _Cols.statusW is sized so that neither actually happens.
      child: Text(text, maxLines: 1, softWrap: false,
          style: TextStyle(color: fg, fontSize: 10.5, fontWeight: FontWeight.w600)),
    );
  }
}
