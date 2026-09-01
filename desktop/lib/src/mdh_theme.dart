import 'package:flutter/material.dart';

/// The MDH-style palette, values taken verbatim from the MDH app
/// (rossum-sa-extension `src/console/console.css`), so the app and the approved
/// proposal stay in lock-step. Light + dark.
@immutable
class MdhColors {
  const MdhColors({
    required this.bgBase,
    required this.bgCard,
    required this.bgSidebar,
    required this.bgHover,
    required this.bgCode,
    required this.textPrimary,
    required this.textSecondary,
    required this.textHint,
    required this.border,
    required this.borderCard,
    required this.accent,
    required this.accentHover,
    required this.successBg,
    required this.successFg,
    required this.successBorder,
    required this.warningBg,
    required this.warningFg,
    required this.warningBorder,
    required this.danger,
    required this.dangerBg,
    required this.dangerFg,
    required this.dangerBorder,
    required this.infoBg,
    required this.infoFg,
    required this.infoBorder,
    required this.extBg,
    required this.extFg,
  });

  final Color bgBase, bgCard, bgSidebar, bgHover, bgCode;
  final Color textPrimary, textSecondary, textHint, border, borderCard;
  final Color accent, accentHover;
  final Color successBg, successFg, successBorder;
  final Color warningBg, warningFg, warningBorder;
  final Color danger, dangerBg, dangerFg, dangerBorder;
  final Color infoBg, infoFg, infoBorder;
  final Color extBg, extFg;

  static const light = MdhColors(
    bgBase: Color(0xFFF1F1F5),
    bgCard: Color(0xFFFFFFFF),
    bgSidebar: Color(0xFFF8F8FB),
    bgHover: Color(0xFFE8E8EE),
    bgCode: Color(0xFFF5F5F8),
    textPrimary: Color(0xFF1A1A24),
    textSecondary: Color(0xFF7A7A8C),
    textHint: Color(0xFF9D9DAB),
    border: Color(0xFFDCDCE4),
    borderCard: Color(0xFFE2E2EA),
    accent: Color(0xFF4270DB),
    accentHover: Color(0xFF3560C5),
    successBg: Color(0xFFD1FAE5),
    successFg: Color(0xFF065F46),
    successBorder: Color(0xFFA7F3D0),
    warningBg: Color(0xFFFEF3C7),
    warningFg: Color(0xFF92400E),
    warningBorder: Color(0xFFFDE68A),
    danger: Color(0xFFCC3333),
    dangerBg: Color(0xFFFEF2F2),
    dangerFg: Color(0xFF991B1B),
    dangerBorder: Color(0xFFFECACA),
    infoBg: Color(0xFFEAF1FD),
    infoFg: Color(0xFF1E40AF),
    infoBorder: Color(0xFFC7DAF8),
    extBg: Color(0xFFEDE9FE),
    extFg: Color(0xFF5B21B6),
  );

  static const dark = MdhColors(
    bgBase: Color(0xFF12121E),
    bgCard: Color(0xFF1A1A2E),
    bgSidebar: Color(0xFF1A1A2E),
    bgHover: Color(0xFF2A2A3E),
    bgCode: Color(0xFF0D0D18),
    textPrimary: Color(0xFFDDDDE8),
    textSecondary: Color(0xFF8888A0),
    textHint: Color(0xFF6A6A80),
    border: Color(0xFF333348),
    borderCard: Color(0xFF2A2A40),
    accent: Color(0xFF5B8AF0),
    accentHover: Color(0xFF6D9AFF),
    successBg: Color(0xFF0A3020),
    successFg: Color(0xFF34D058),
    successBorder: Color(0xFF1A4A30),
    warningBg: Color(0xFF3A2F10),
    warningFg: Color(0xFFF59E0B),
    warningBorder: Color(0xFF4A3A10),
    danger: Color(0xFFEF5555),
    dangerBg: Color(0xFF2A1515),
    dangerFg: Color(0xFFFCA5A5),
    dangerBorder: Color(0xFF4A2020),
    infoBg: Color(0xFF15233F),
    infoFg: Color(0xFF93B4F5),
    infoBorder: Color(0xFF294063),
    extBg: Color(0xFF2A1F4A),
    extFg: Color(0xFFC4B5FD),
  );

  static MdhColors of(BuildContext context) =>
      Theme.of(context).brightness == Brightness.dark ? dark : light;
}

/// Monospace stack, used only for technical values (host, org id, path, log).
const String kMonoFamily = 'Menlo';
const List<String> kMonoFallback = <String>[
  'SF Mono', 'SFMono-Regular', 'Monaco', 'Consolas', 'Cascadia Mono',
  'Ubuntu Mono', 'DejaVu Sans Mono', 'monospace',
];

/// Monospace text style for technical values (host, org id, path, log).
/// Shared by [home_page.dart] and [dialogs.dart] — lift changes here, don't
/// re-add a per-file copy.
TextStyle monoStyle(Color color, double size, [FontWeight w = FontWeight.w400]) =>
    TextStyle(color: color, fontSize: size, fontWeight: w,
        fontFamily: kMonoFamily, fontFamilyFallback: kMonoFallback);

/// A small outlined/filled button matching the MDH look, used throughout the
/// app's custom widgets (not stock Material buttons). Shared by
/// [home_page.dart] and [dialogs.dart] — lift changes here, don't re-add a
/// per-file copy.
class MdhBtn extends StatelessWidget {
  const MdhBtn({super.key, required this.label, this.primary = false, this.onTap});
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
        mouseCursor: onTap == null ? SystemMouseCursors.basic : SystemMouseCursors.click,
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

/// Material theme hosting the MDH look: system sans by default, the palette
/// above, light/dark following the OS. Most UI is custom widgets reading
/// [MdhColors.of]; this sets backgrounds, text color, and accent so stock bits
/// (dialogs, snackbars, scrollbars) blend in.
ThemeData mdhTheme(Brightness brightness) {
  final c = brightness == Brightness.dark ? MdhColors.dark : MdhColors.light;
  final base = ThemeData(brightness: brightness, useMaterial3: true);
  return base.copyWith(
    scaffoldBackgroundColor: c.bgBase,
    canvasColor: c.bgBase,
    dividerColor: c.border,
    colorScheme: base.colorScheme.copyWith(
      primary: c.accent,
      onPrimary: Colors.white,
      surface: c.bgCard,
      onSurface: c.textPrimary,
      error: c.danger,
    ),
    textTheme: base.textTheme.apply(bodyColor: c.textPrimary, displayColor: c.textPrimary),
    snackBarTheme: SnackBarThemeData(
      backgroundColor: c.bgCard,
      contentTextStyle: TextStyle(color: c.textPrimary),
      behavior: SnackBarBehavior.floating,
    ),
  );
}
