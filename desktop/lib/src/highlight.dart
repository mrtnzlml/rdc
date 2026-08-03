import 'package:flutter/material.dart';

import 'mdh_theme.dart';

// Lightweight, dependency-free syntax highlighting for the read-only file
// preview. Deliberately "basic": enough to make the file types that show up in
// an rdc snapshot readable — JSON (dominant), Python (.py sidecars), TOML
// (config), and Markdown — themed with the MDH palette. Anything else, or a
// very large file, renders as plain text. Every character is preserved so the
// text stays faithfully selectable/copyable.

const int _maxHighlightChars = 200000;

enum _Lang { json, python, toml, markdown, none }

enum _K { plain, key, str, num, kw, comment, punct, heading, code, marker }

class _Tok {
  const _Tok(this.text, this.kind);
  final String text;
  final _K kind;
}

/// Builds themed [TextSpan]s for [code], choosing a grammar from [ext]
/// (a lower-case extension without the dot, e.g. `json`).
List<TextSpan> highlightSource(String code, String? ext, MdhColors c, double size) {
  final base = TextStyle(
    color: c.textPrimary,
    fontSize: size,
    fontFamily: kMonoFamily,
    fontFamilyFallback: kMonoFallback,
  );
  if (code.length > _maxHighlightChars) return [TextSpan(text: code, style: base)];
  final toks = switch (_langFor(ext)) {
    _Lang.json => _json(code),
    _Lang.python => _python(code),
    _Lang.toml => _toml(code),
    _Lang.markdown => _markdown(code),
    _Lang.none => const <_Tok>[],
  };
  if (toks.isEmpty) return [TextSpan(text: code, style: base)];
  return [for (final t in toks) TextSpan(text: t.text, style: _styleFor(t.kind, base, c))];
}

_Lang _langFor(String? ext) {
  switch (ext) {
    case 'json':
      return _Lang.json;
    case 'py':
    case 'pyi':
      return _Lang.python;
    case 'toml':
    case 'ini':
    case 'cfg':
      return _Lang.toml;
    case 'md':
    case 'markdown':
      return _Lang.markdown;
    default:
      return _Lang.none;
  }
}

TextStyle _styleFor(_K k, TextStyle base, MdhColors c) {
  switch (k) {
    case _K.plain:
      return base;
    case _K.key:
      return base.copyWith(color: c.accent);
    case _K.str:
      return base.copyWith(color: c.successFg);
    case _K.num:
      return base.copyWith(color: c.warningFg);
    case _K.kw:
      return base.copyWith(color: c.extFg);
    case _K.comment:
      return base.copyWith(color: c.textHint, fontStyle: FontStyle.italic);
    case _K.punct:
      return base.copyWith(color: c.textSecondary);
    case _K.heading:
      return base.copyWith(color: c.accent, fontWeight: FontWeight.w700);
    case _K.code:
      return base.copyWith(color: c.infoFg);
    case _K.marker:
      return base.copyWith(color: c.warningFg);
  }
}

bool _isDigit(String ch) => ch.codeUnitAt(0) >= 0x30 && ch.codeUnitAt(0) <= 0x39;
bool _isAlpha(String ch) {
  final u = ch.codeUnitAt(0);
  return (u >= 0x41 && u <= 0x5A) || (u >= 0x61 && u <= 0x7A) || u == 0x5F; // A-Z a-z _
}

bool _isAlnum(String ch) => _isAlpha(ch) || _isDigit(ch);
bool _isSpace(String ch) => ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r';

// ------------------------------------------------------------ JSON

List<_Tok> _json(String s) {
  final out = <_Tok>[];
  final n = s.length;
  var i = 0, plain = -1;
  void flush(int e) {
    if (plain >= 0 && e > plain) out.add(_Tok(s.substring(plain, e), _K.plain));
    plain = -1;
  }

  while (i < n) {
    final ch = s[i];
    if (ch == '"') {
      flush(i);
      final start = i++;
      while (i < n) {
        if (s[i] == '\\' && i + 1 < n) {
          i += 2;
          continue;
        }
        if (s[i] == '"' || s[i] == '\n') {
          if (s[i] == '"') i++;
          break;
        }
        i++;
      }
      var j = i;
      while (j < n && _isSpace(s[j])) {
        j++;
      }
      out.add(_Tok(s.substring(start, i), (j < n && s[j] == ':') ? _K.key : _K.str));
    } else if (_isDigit(ch) || (ch == '-' && i + 1 < n && _isDigit(s[i + 1]))) {
      flush(i);
      final start = i++;
      while (i < n && (_isDigit(s[i]) || s[i] == '.' || s[i] == 'e' || s[i] == 'E' || s[i] == '+' || s[i] == '-')) {
        i++;
      }
      out.add(_Tok(s.substring(start, i), _K.num));
    } else if (_isAlpha(ch)) {
      flush(i);
      final start = i;
      while (i < n && _isAlpha(s[i])) {
        i++;
      }
      final w = s.substring(start, i);
      out.add(_Tok(w, (w == 'true' || w == 'false' || w == 'null') ? _K.kw : _K.plain));
    } else if ('{}[]:,'.contains(ch)) {
      flush(i);
      out.add(_Tok(ch, _K.punct));
      i++;
    } else {
      if (plain < 0) plain = i;
      i++;
    }
  }
  flush(i);
  return out;
}

// ------------------------------------------------------------ Python

const Set<String> _pyKw = {
  'and', 'as', 'assert', 'async', 'await', 'break', 'class', 'continue', 'def',
  'del', 'elif', 'else', 'except', 'finally', 'for', 'from', 'global', 'if',
  'import', 'in', 'is', 'lambda', 'nonlocal', 'not', 'or', 'pass', 'raise',
  'return', 'try', 'while', 'with', 'yield', 'None', 'True', 'False',
};

List<_Tok> _python(String s) {
  final out = <_Tok>[];
  final n = s.length;
  var i = 0, plain = -1;
  void flush(int e) {
    if (plain >= 0 && e > plain) out.add(_Tok(s.substring(plain, e), _K.plain));
    plain = -1;
  }

  while (i < n) {
    final ch = s[i];
    if (ch == '#') {
      flush(i);
      final start = i;
      while (i < n && s[i] != '\n') {
        i++;
      }
      out.add(_Tok(s.substring(start, i), _K.comment));
    } else if (ch == '"' || ch == "'") {
      flush(i);
      final start = i;
      final triple = i + 2 < n && s[i + 1] == ch && s[i + 2] == ch;
      if (triple) {
        i += 3;
        while (i < n) {
          if (s[i] == ch && i + 2 < n && s[i + 1] == ch && s[i + 2] == ch) {
            i += 3;
            break;
          }
          i++;
        }
      } else {
        i++;
        while (i < n && s[i] != '\n') {
          if (s[i] == '\\' && i + 1 < n) {
            i += 2;
            continue;
          }
          if (s[i] == ch) {
            i++;
            break;
          }
          i++;
        }
      }
      out.add(_Tok(s.substring(start, i), _K.str));
    } else if (_isDigit(ch)) {
      flush(i);
      final start = i++;
      while (i < n && (_isAlnum(s[i]) || s[i] == '.')) {
        i++;
      }
      out.add(_Tok(s.substring(start, i), _K.num));
    } else if (_isAlpha(ch)) {
      flush(i);
      final start = i;
      while (i < n && _isAlnum(s[i])) {
        i++;
      }
      final w = s.substring(start, i);
      out.add(_Tok(w, _pyKw.contains(w) ? _K.kw : _K.plain));
    } else {
      if (plain < 0) plain = i;
      i++;
    }
  }
  flush(i);
  return out;
}

// ------------------------------------------------------------ TOML / INI

List<_Tok> _toml(String s) {
  final out = <_Tok>[];
  final n = s.length;
  var i = 0, plain = -1;
  void flush(int e) {
    if (plain >= 0 && e > plain) out.add(_Tok(s.substring(plain, e), _K.plain));
    plain = -1;
  }

  bool bareKeyChar(String ch) => _isAlnum(ch) || ch == '-' || ch == '.';

  while (i < n) {
    final ch = s[i];
    if (ch == '#' || ch == ';') {
      flush(i);
      final start = i;
      while (i < n && s[i] != '\n') {
        i++;
      }
      out.add(_Tok(s.substring(start, i), _K.comment));
    } else if (ch == '"' || ch == "'") {
      flush(i);
      final start = i++;
      while (i < n && s[i] != '\n') {
        if (s[i] == '\\' && i + 1 < n && ch == '"') {
          i += 2;
          continue;
        }
        if (s[i] == ch) {
          i++;
          break;
        }
        i++;
      }
      var j = i;
      while (j < n && (s[j] == ' ' || s[j] == '\t')) {
        j++;
      }
      out.add(_Tok(s.substring(start, i), (j < n && s[j] == '=') ? _K.key : _K.str));
    } else if (_isDigit(ch) || (ch == '-' && i + 1 < n && _isDigit(s[i + 1]))) {
      flush(i);
      final start = i++;
      while (i < n && (_isAlnum(s[i]) || s[i] == '.' || s[i] == ':' || s[i] == '+' || s[i] == '-')) {
        i++;
      }
      out.add(_Tok(s.substring(start, i), _K.num));
    } else if (_isAlpha(ch)) {
      flush(i);
      final start = i;
      while (i < n && bareKeyChar(s[i])) {
        i++;
      }
      final w = s.substring(start, i);
      var j = i;
      while (j < n && (s[j] == ' ' || s[j] == '\t')) {
        j++;
      }
      final kind = (j < n && s[j] == '=')
          ? _K.key
          : (w == 'true' || w == 'false')
              ? _K.kw
              : _K.plain;
      out.add(_Tok(w, kind));
    } else {
      if (plain < 0) plain = i;
      i++;
    }
  }
  flush(i);
  return out;
}

// ------------------------------------------------------------ Markdown

final RegExp _mdHeading = RegExp(r'^#{1,6}\s');
final RegExp _mdList = RegExp(r'^(\s*)([-*+]|\d+\.)(\s)');

List<_Tok> _markdown(String s) {
  final out = <_Tok>[];
  final lines = s.split('\n');
  var inFence = false;
  for (var li = 0; li < lines.length; li++) {
    final line = lines[li];
    final nl = li < lines.length - 1 ? '\n' : '';
    final trimmed = line.trimLeft();
    if (trimmed.startsWith('```') || trimmed.startsWith('~~~')) {
      inFence = !inFence;
      out.add(_Tok(line + nl, _K.code));
      continue;
    }
    if (inFence) {
      out.add(_Tok(line + nl, _K.code));
      continue;
    }
    if (_mdHeading.hasMatch(trimmed)) {
      out.add(_Tok(line + nl, _K.heading));
      continue;
    }
    final m = _mdList.firstMatch(line);
    if (m != null) {
      out.add(_Tok(m.group(1)!, _K.plain));
      out.add(_Tok(m.group(2)!, _K.marker));
      out.add(_Tok(m.group(3)!, _K.plain));
      _mdInline(line.substring(m.end), out);
    } else {
      _mdInline(line, out);
    }
    if (nl.isNotEmpty) out.add(const _Tok('\n', _K.plain));
  }
  return out;
}

void _mdInline(String t, List<_Tok> out) {
  var i = 0;
  final n = t.length;
  while (i < n) {
    if (t[i] == '`') {
      final start = i++;
      while (i < n && t[i] != '`') {
        i++;
      }
      if (i < n) i++; // include closing backtick
      out.add(_Tok(t.substring(start, i), _K.code));
    } else {
      final start = i;
      while (i < n && t[i] != '`') {
        i++;
      }
      out.add(_Tok(t.substring(start, i), _K.plain));
    }
  }
}
