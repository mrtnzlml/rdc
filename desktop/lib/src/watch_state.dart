import 'rust/api/rdc.dart';

/// A prompt a cycle is blocked on, tagged with the env it came from so the
/// dialog can name it and the answer can be routed back.
class PendingPrompt {
  const PendingPrompt({
    required this.id,
    required this.kind,
    required this.question,
    required this.keys,
    required this.folder,
    required this.env,
  });

  final BigInt id;
  final PromptKindDto kind;
  final String question;
  final List<PromptChoice> keys;
  final String folder;
  final String env;

  /// Dialog title. The question line itself is shown verbatim underneath.
  String get title => switch (kind) {
        PromptKindDto.conflict => 'Changed in both places',
        PromptKindDto.remoteDelete => 'Deleted on one side',
        PromptKindDto.bulkConfirm => 'Apply to all?',
        PromptKindDto.deleteGate => 'Delete from Rossum?',
        PromptKindDto.deleteDrift => 'Deleted locally, changed remotely',
        PromptKindDto.mdhIndexDrop => 'Drop indexes?',
        PromptKindDto.mdhRowDelete => 'Delete rows?',
        PromptKindDto.unknown => 'Unrecognised prompt',
      };
}

/// Per-env watch state. Absent from `AppState.watch` means "not watching".
class WatchState {
  WatchState({this.running = false, this.nextPollSecs});
  bool running;
  int? nextPollSecs;
}
