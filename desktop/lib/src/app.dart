import 'package:flutter/material.dart';

import 'app_state.dart';
import 'home_page.dart';

class RdcApp extends StatelessWidget {
  const RdcApp({super.key, required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    const seed = Color(0xFF3B5BFF);
    return MaterialApp(
      title: 'rdc',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(colorSchemeSeed: seed, useMaterial3: true),
      darkTheme: ThemeData(
        colorSchemeSeed: seed,
        brightness: Brightness.dark,
        useMaterial3: true,
      ),
      home: HomePage(state: state),
    );
  }
}
