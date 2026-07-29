import 'package:flutter/material.dart';

import 'app_state.dart';
import 'home_page.dart';
import 'mdh_theme.dart';

class RdcApp extends StatelessWidget {
  const RdcApp({super.key, required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'rdc',
      debugShowCheckedModeBanner: false,
      theme: mdhTheme(Brightness.light),
      darkTheme: mdhTheme(Brightness.dark),
      home: HomePage(state: state),
    );
  }
}
