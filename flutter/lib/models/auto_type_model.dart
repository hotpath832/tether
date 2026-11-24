import 'package:get/get.dart';

import 'model.dart';
import 'platform_model.dart';

/// Tether: state of the "Auto type" job of one remote session.
/// The typing itself runs in Rust (src/auto_type.rs); it reports through
/// "auto_type" session events.
class AutoTypeModel {
  final WeakReference<FFI> parent;

  /// '' (idle), countdown, running, paused, done, stopped, error.
  final state = ''.obs;
  final done = 0.obs;
  final total = 0.obs;

  /// Countdown seconds left, or the reason for paused / error.
  final extra = ''.obs;

  /// Last text typed, offered again the next time the dialog opens.
  String lastText = '';

  AutoTypeModel(this.parent);

  bool get isActive =>
      state.value == 'countdown' ||
      state.value == 'running' ||
      state.value == 'paused';

  void handleEvent(Map<String, dynamic> evt) {
    final s = evt['state'];
    if (s is! String) return;
    final d = int.tryParse(evt['done']?.toString() ?? '');
    final t = int.tryParse(evt['total']?.toString() ?? '');
    if (d != null) done.value = d;
    if (t != null) total.value = t;
    extra.value = evt['extra']?.toString() ?? '';
    state.value = s;
  }

  void start(String text,
      {required double wpm,
      required double errorRate,
      required int startDelay,
      required bool thinkPauses}) {
    final ffi = parent.target;
    if (ffi == null) return;
    lastText = text;
    done.value = 0;
    total.value = text.length;
    extra.value = '';
    state.value = startDelay > 0 ? 'countdown' : 'running';
    if (startDelay > 0) extra.value = '$startDelay';
    bind.sessionAutoTypeStart(
        sessionId: ffi.sessionId,
        text: text,
        wpm: wpm,
        errorRate: errorRate,
        startDelay: startDelay,
        thinkPauses: thinkPauses);
  }

  void pause() => _control('pause');
  void resume() => _control('resume');
  void stop() => _control('stop');

  /// Hide the status bar after the job ended.
  void dismiss() {
    if (!isActive) state.value = '';
  }

  void _control(String action) {
    final ffi = parent.target;
    if (ffi == null) return;
    bind.sessionAutoTypeControl(sessionId: ffi.sessionId, action: action);
  }
}
