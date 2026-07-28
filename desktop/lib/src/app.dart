import 'package:flutter/material.dart';

import 'app_state.dart';
import 'console_theme.dart';
import 'home_page.dart';

class RdcApp extends StatelessWidget {
  const RdcApp({super.key, required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'rdc',
      debugShowCheckedModeBanner: false,
      theme: consoleTheme(Brightness.light),
      darkTheme: consoleTheme(Brightness.dark),
      home: HomePage(state: state),
    );
  }
}
