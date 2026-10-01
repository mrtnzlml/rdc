import 'dart:convert';
import 'dart:io';

/// The desktop app's own version (independent of the embedded rdc core
/// version). Bumped alongside `desktop-v*` release tags.
const String kAppVersion = '0.1.0';

class UpdateInfo {
  final String latest;
  final String url;
  const UpdateInfo(this.latest, this.url);
}

/// Best-effort launch-time check for a newer `desktop-v*` GitHub release.
///
/// The rdc repository is public, so the unauthenticated release query
/// succeeds. NOTE: no `desktop-v*` release exists — the app ships as
/// `rdc-desktop-*` assets on the CLI's `v*` releases — so this currently
/// surfaces nothing. It degrades gracefully: any non-200, timeout, or parse
/// failure yields `null` (no update surfaced) and never throws.
Future<UpdateInfo?> checkForUpdate() async {
  HttpClient? client;
  try {
    client = HttpClient()..connectionTimeout = const Duration(seconds: 6);
    final req = await client.getUrl(
      Uri.parse('https://api.github.com/repos/mrtnzlml/rdc/releases?per_page=30'),
    );
    req.headers.set(HttpHeaders.userAgentHeader, 'rossum-local');
    req.headers.set(HttpHeaders.acceptHeader, 'application/vnd.github+json');
    final resp = await req.close().timeout(const Duration(seconds: 8));
    if (resp.statusCode != 200) return null;
    final body = await resp.transform(utf8.decoder).join();
    final releases = (jsonDecode(body) as List).cast<Map<String, dynamic>>();
    for (final rel in releases) {
      final tag = (rel['tag_name'] as String?) ?? '';
      if (!tag.startsWith('desktop-v')) continue;
      final latest = tag.substring('desktop-v'.length);
      if (_isNewer(latest, kAppVersion)) {
        return UpdateInfo(latest, (rel['html_url'] as String?) ?? '');
      }
      break; // releases are newest-first; first desktop-v* is the latest
    }
  } catch (_) {
    // network/parse errors → no update surfaced
  } finally {
    client?.close(force: true);
  }
  return null;
}

/// Minimal dotted-numeric version compare (ignores pre-release suffixes).
bool _isNewer(String a, String b) {
  List<int> parts(String v) => v
      .split(RegExp(r'[.\-+]'))
      .map((s) => int.tryParse(s) ?? 0)
      .toList();
  final pa = parts(a), pb = parts(b);
  for (var i = 0; i < pa.length || i < pb.length; i++) {
    final x = i < pa.length ? pa[i] : 0;
    final y = i < pb.length ? pb[i] : 0;
    if (x != y) return x > y;
  }
  return false;
}
