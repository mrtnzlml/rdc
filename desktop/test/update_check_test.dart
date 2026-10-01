import 'package:desktop/src/update_check.dart';
import 'package:flutter_test/flutter_test.dart';

Map<String, dynamic> release(String tag, List<String> assets) => {
      'tag_name': tag,
      'html_url': 'https://github.com/mrtnzlml/rdc/releases/tag/$tag',
      'assets': [for (final a in assets) {'name': a}],
    };

void main() {
  const desktop = ['rdc-0.13.0-x86_64-apple-darwin.tar.gz', 'rdc-desktop-0.13.0.dmg'];

  test('a newer release with desktop assets is an update', () {
    final info = updateFrom(release('v0.13.0', desktop), '0.12.0');
    expect(info?.latest, '0.13.0');
    expect(info?.url, 'https://github.com/mrtnzlml/rdc/releases/tag/v0.13.0');
  });

  test('the same or an older release is not', () {
    expect(updateFrom(release('v0.13.0', desktop), '0.13.0'), isNull);
    expect(updateFrom(release('v0.13.0', desktop), '0.13.1'), isNull);
  });

  test('versions compare numerically, not as strings', () {
    expect(updateFrom(release('v0.13.0', desktop), '0.9.0')?.latest, '0.13.0');
  });

  test('a release whose desktop build failed is not an update', () {
    expect(updateFrom(release('v0.13.0', ['rdc-0.13.0-x86_64-apple-darwin.tar.gz']), '0.12.0'),
        isNull);
  });
}
