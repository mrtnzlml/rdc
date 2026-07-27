import 'package:flutter_rust_bridge/flutter_rust_bridge.dart';

/// Turns any thrown error into a clean, human-readable sentence for the UI.
///
/// Rust bridge errors arrive as [AnyhowException], whose `toString()` is
/// `AnyhowException(<message>)` — we show just the `<message>`. Dart's own
/// exceptions stringify as `Exception: <message>` — we strip that prefix too.
/// The underlying text is preserved (the UI keeps it selectable/copyable), we
/// only remove the technical wrapper so users don't see `AnyhowException(...)`.
String errorText(Object error) {
  String msg;
  if (error is AnyhowException) {
    msg = error.message;
  } else {
    msg = error.toString();
  }
  msg = msg
      .replaceFirst(RegExp(r'^(_?Exception|StateError|ArgumentError):\s*'), '')
      .trim();
  return msg.isEmpty ? 'Something went wrong.' : msg;
}
