import 'package:flutter/material.dart';

/// The Console design palette. Values are copied verbatim from the approved
/// mockup's `--c-*` tokens (see docs/superpowers/specs and the design artifact),
/// so the app and the proposal stay in lock-step.
@immutable
class ConsoleColors {
  const ConsoleColors({
    required this.bg,
    required this.surf,
    required this.log,
    required this.ink,
    required this.muted,
    required this.line,
    required this.acc,
    required this.ok,
    required this.err,
    required this.sel,
  });

  final Color bg, surf, log, ink, muted, line, acc, ok, err, sel;

  static const light = ConsoleColors(
    bg: Color(0xFFF4EFE4),
    surf: Color(0xFFFBF8F1),
    log: Color(0xFFEFE8D8),
    ink: Color(0xFF241F17),
    muted: Color(0xFF8A8072),
    line: Color(0xFFE3DAC7),
    acc: Color(0xFFBD611B),
    ok: Color(0xFF2F7D3A),
    err: Color(0xFFC0392B),
    sel: Color(0xFFF0E4D0),
  );

  static const dark = ConsoleColors(
    bg: Color(0xFF0E0C0A),
    surf: Color(0xFF16130F),
    log: Color(0xFF0A0806),
    ink: Color(0xFFECE3D3),
    muted: Color(0xFF8F8676),
    line: Color(0xFF2B2620),
    acc: Color(0xFFE0913F),
    ok: Color(0xFF79C081),
    err: Color(0xFFEF7B6A),
    sel: Color(0xFF221B12),
  );

  static ConsoleColors of(BuildContext context) =>
      Theme.of(context).brightness == Brightness.dark ? dark : light;
}

/// System monospace stack, mirroring the mockup's `ui-monospace, "SF Mono",
/// "Menlo", "Consolas", …` so the rendered face matches the proposal.
const String kMonoFamily = 'Menlo';
const List<String> kMonoFallback = <String>[
  'SF Mono',
  'SFMono-Regular',
  'Monaco',
  'Consolas',
  'Cascadia Mono',
  'Ubuntu Mono',
  'DejaVu Sans Mono',
  'monospace',
];

/// A Material theme that hosts the Console look: monospace everywhere, the
/// palette above, and light/dark that follow the OS. Most of the UI is drawn
/// with custom widgets that read [ConsoleColors.of]; this theme sets the
/// backgrounds, text color, and accents so stock bits (dialogs, snackbars,
/// scrollbars) blend in.
ThemeData consoleTheme(Brightness brightness) {
  final c = brightness == Brightness.dark ? ConsoleColors.dark : ConsoleColors.light;
  final base = ThemeData(
    brightness: brightness,
    useMaterial3: true,
    fontFamily: kMonoFamily,
    fontFamilyFallback: kMonoFallback,
  );
  return base.copyWith(
    scaffoldBackgroundColor: c.bg,
    canvasColor: c.bg,
    dividerColor: c.line,
    colorScheme: base.colorScheme.copyWith(
      primary: c.acc,
      onPrimary: brightness == Brightness.dark ? const Color(0xFF17130D) : Colors.white,
      surface: c.surf,
      onSurface: c.ink,
      error: c.err,
    ),
    textTheme: base.textTheme.apply(
      bodyColor: c.ink,
      displayColor: c.ink,
      fontFamily: kMonoFamily,
      fontFamilyFallback: kMonoFallback,
    ),
    snackBarTheme: SnackBarThemeData(
      backgroundColor: c.surf,
      contentTextStyle: TextStyle(color: c.ink, fontFamily: kMonoFamily, fontFamilyFallback: kMonoFallback),
      behavior: SnackBarBehavior.floating,
    ),
  );
}
