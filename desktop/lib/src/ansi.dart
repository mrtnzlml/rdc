import 'package:flutter/material.dart';

import 'console_theme.dart';

final _ansi = RegExp('\x1B\\[([0-9;]*)m');

/// Parse an rdc log line containing ANSI SGR color codes into styled spans,
/// mapping rdc's semantic truecolors to the console palette (so the log keeps
/// rdc's coloring while matching the app's light/dark theme). Any escape codes
/// are consumed as style changes — never rendered as text.
///
/// rdc's SGR set (src/cli/resolve.rs): dim `2` (timestamps), and truecolor
/// `38;2;r;g;b` for amber 237,142,71 (accent), red 220,80,80 (error), and
/// green 120,180,90 (added/ok); `1` = bold; `0` = reset.
List<InlineSpan> ansiSpans(String line, ConsoleColors c, double fontSize) {
  final spans = <InlineSpan>[];
  var color = c.ink;
  var bold = false;
  var i = 0;

  void emit(String text) {
    if (text.isEmpty) return;
    spans.add(TextSpan(
      text: text,
      style: TextStyle(
        color: color,
        fontWeight: bold ? FontWeight.w600 : FontWeight.w400,
        fontSize: fontSize,
      ),
    ));
  }

  for (final m in _ansi.allMatches(line)) {
    if (m.start > i) emit(line.substring(i, m.start));
    final parts = m.group(1)!.split(';');
    if (m.group(1)!.isEmpty || parts.contains('0')) {
      color = c.ink;
      bold = false;
    }
    if (parts.contains('1')) bold = true;
    if (parts.contains('2')) color = c.muted; // dim
    final t = parts.indexOf('38');
    if (t >= 0 && t + 4 < parts.length && parts[t + 1] == '2') {
      final r = int.tryParse(parts[t + 2]) ?? 0;
      final g = int.tryParse(parts[t + 3]) ?? 0;
      final b = int.tryParse(parts[t + 4]) ?? 0;
      color = _semantic(r, g, b, c);
    }
    i = m.end;
  }
  if (i < line.length) emit(line.substring(i));
  if (spans.isEmpty) emit(line);
  return spans;
}

/// Map rdc's fixed truecolors to the palette; unknown → ink.
Color _semantic(int r, int g, int b, ConsoleColors c) {
  if (r == 237 && g == 142 && b == 71) return c.acc; // amber
  if (r == 220 && g == 80 && b == 80) return c.err; // red
  if (r == 120 && g == 180 && b == 90) return c.ok; // green
  return c.ink;
}
