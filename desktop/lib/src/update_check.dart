import 'dart:convert';
import 'dart:io';

class UpdateInfo {
  final String latest;
  final String url;
  const UpdateInfo(this.latest, this.url);
}

/// Best-effort check for a newer release than [current], the embedded rdc
/// core's version (`rdcVersion()`).
///
/// The desktop app ships as `rdc-desktop-*` assets on the CLI's `v*` releases,
/// and the core's version is the release version, so the latest release is
/// the one to compare against. It degrades gracefully: any non-200, timeout,
/// or parse failure yields `null` (no update surfaced) and never throws.
Future<UpdateInfo?> checkForUpdate(String current) async {
  HttpClient? client;
  try {
    client = HttpClient()..connectionTimeout = const Duration(seconds: 6);
    final req = await client.getUrl(
      Uri.parse('https://api.github.com/repos/mrtnzlml/rdc/releases/latest'),
    );
    req.headers.set(HttpHeaders.userAgentHeader, 'rossum-local');
    req.headers.set(HttpHeaders.acceptHeader, 'application/vnd.github+json');
    final resp = await req.close().timeout(const Duration(seconds: 8));
    if (resp.statusCode != 200) return null;
    final body = await resp.transform(utf8.decoder).join();
    return updateFrom(jsonDecode(body) as Map<String, dynamic>, current);
  } catch (_) {
    // network/parse errors → no update surfaced
  } finally {
    client?.close(force: true);
  }
  return null;
}

/// The update [release] (a GitHub release object) offers over [current], or
/// `null`. A release without desktop assets offers none: the release job
/// publishes the CLI even when the desktop build failed.
UpdateInfo? updateFrom(Map<String, dynamic> release, String current) {
  final tag = (release['tag_name'] as String?) ?? '';
  if (!tag.startsWith('v')) return null;
  final assets = (release['assets'] as List?) ?? const [];
  final hasDesktop = assets.any((a) =>
      ((a as Map)['name'] as String? ?? '').startsWith('rdc-desktop-'));
  if (!hasDesktop) return null;
  final latest = tag.substring(1);
  if (!_isNewer(latest, current)) return null;
  return UpdateInfo(latest, (release['html_url'] as String?) ?? '');
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
