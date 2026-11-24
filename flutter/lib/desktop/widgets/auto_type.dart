// Tether: "Auto type" dialog and the status bar shown while it types.
import 'package:flutter/material.dart';
import 'package:get/get.dart';

import '../../common.dart';
import '../../models/model.dart';
import '../../models/platform_model.dart';

const _kOptWpm = 'auto-type-wpm';
const _kOptMistakes = 'auto-type-mistakes';
const _kOptDelay = 'auto-type-delay';
const _kOptThink = 'auto-type-think-pauses';

double _readNum(String key, double fallback) =>
    double.tryParse(bind.mainGetLocalOption(key: key)) ?? fallback;

void showAutoTypeDialog(FFI ffi) {
  final model = ffi.autoTypeModel;
  final textController = TextEditingController(text: model.lastText);
  final wpmController = TextEditingController(
      text: _readNum(_kOptWpm, 60).round().toString());
  final mistakesController = TextEditingController(
      text: _fmt(_readNum(_kOptMistakes, 3)));
  final delayController = TextEditingController(
      text: _readNum(_kOptDelay, 3).round().toString());
  var thinkPauses = bind.mainGetLocalOption(key: _kOptThink) != 'N';
  String? error;

  ffi.dialogManager.show((setState, close, context) {
    submit() {
      final text = textController.text;
      final wpm = double.tryParse(wpmController.text.trim());
      final mistakes = double.tryParse(mistakesController.text.trim());
      final delay = int.tryParse(delayController.text.trim());
      String? err;
      if (text.isEmpty) {
        err = translate('There is no text to type');
      } else if (wpm == null || wpm < 5 || wpm > 300) {
        err = '${translate('Speed (WPM)')}: 5 - 300';
      } else if (mistakes == null || mistakes < 0 || mistakes > 30) {
        err = '${translate('Mistakes (%)')}: 0 - 30';
      } else if (delay == null || delay < 0 || delay > 30) {
        err = '${translate('Start delay (s)')}: 0 - 30';
      }
      if (err != null) {
        setState(() => error = err);
        return;
      }
      bind.mainSetLocalOption(key: _kOptWpm, value: wpm!.round().toString());
      bind.mainSetLocalOption(key: _kOptMistakes, value: _fmt(mistakes!));
      bind.mainSetLocalOption(key: _kOptDelay, value: delay.toString());
      bind.mainSetLocalOption(key: _kOptThink, value: thinkPauses ? '' : 'N');
      close();
      model.start(text,
          wpm: wpm.roundToDouble(),
          errorRate: mistakes / 100,
          startDelay: delay!,
          thinkPauses: thinkPauses);
    }

    Widget numField(String label, TextEditingController c) => Expanded(
          child: TextField(
            controller: c,
            keyboardType: const TextInputType.numberWithOptions(decimal: true),
            decoration: InputDecoration(labelText: translate(label)),
          ).paddingSymmetric(horizontal: 4),
        );

    return CustomAlertDialog(
      title: Row(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          Icon(Icons.keyboard_alt_outlined, color: MyTheme.accent),
          Text(translate('Auto type')).paddingOnly(left: 10),
        ],
      ),
      contentBoxConstraints: const BoxConstraints(maxWidth: 560),
      content: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(translate('auto-type-tip'),
              style: Theme.of(context).textTheme.bodySmall),
          const SizedBox(height: 8),
          TextField(
            controller: textController,
            autofocus: true,
            minLines: 6,
            maxLines: 12,
            keyboardType: TextInputType.multiline,
            decoration: InputDecoration(
              hintText: translate('Text to type'),
              border: const OutlineInputBorder(),
            ),
          ),
          Row(children: [
            numField('Speed (WPM)', wpmController),
            numField('Mistakes (%)', mistakesController),
            numField('Start delay (s)', delayController),
          ]).paddingOnly(top: 8),
          CheckboxListTile(
            contentPadding: EdgeInsets.zero,
            dense: true,
            controlAffinity: ListTileControlAffinity.leading,
            title: Text(translate('Thinking pauses')),
            value: thinkPauses,
            onChanged: (v) {
              if (v == null) return;
              setState(() => thinkPauses = v);
            },
          ),
          if (error != null)
            Text(error!, style: const TextStyle(color: Colors.red)),
        ],
      ),
      actions: [
        dialogButton('Cancel',
            icon: const Icon(Icons.close_rounded),
            onPressed: close,
            isOutline: true),
        dialogButton('Start',
            icon: const Icon(Icons.play_arrow_rounded), onPressed: submit),
      ],
      // No onSubmit: Enter adds a new line in the text box.
      onCancel: close,
    );
  });
}

String _fmt(double v) =>
    v == v.roundToDouble() ? v.round().toString() : v.toString();

/// Small bar at the bottom of the remote view while auto type runs.
class AutoTypeStatusBar extends StatelessWidget {
  final FFI ffi;
  const AutoTypeStatusBar({Key? key, required this.ffi}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    final m = ffi.autoTypeModel;
    return Obx(() {
      final state = m.state.value;
      if (state.isEmpty) return const Offstage();
      final total = m.total.value;
      final done = m.done.value;
      final percent = total > 0 ? (done * 100 ~/ total) : 0;
      final String label;
      switch (state) {
        case 'countdown':
          label = '${translate('Auto type')}: ${translate('Starting in')} ${m.extra.value}s';
          break;
        case 'running':
          label = '${translate('Auto type')}: $percent% ($done/$total)';
          break;
        case 'paused':
          label = '${translate('Auto type')}: ${translate('Paused')} $percent%'
              '${m.extra.value.isNotEmpty ? ' - ${translate(m.extra.value)}' : ''}';
          break;
        case 'done':
          label = '${translate('Auto type')}: ${translate('Finished')}';
          break;
        case 'stopped':
          label = '${translate('Auto type')}: ${translate('Stopped')} $percent%';
          break;
        default:
          label = '${translate('Auto type')}: ${translate(m.extra.value)}';
      }
      final active = m.isActive;
      Widget btn(IconData icon, String tip, VoidCallback onPressed) => IconButton(
            icon: Icon(icon, size: 18, color: Colors.white),
            tooltip: translate(tip),
            visualDensity: VisualDensity.compact,
            onPressed: onPressed,
          );
      return Align(
        alignment: Alignment.bottomCenter,
        child: Container(
          margin: const EdgeInsets.only(bottom: 16),
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 2),
          decoration: BoxDecoration(
            color: Colors.black.withOpacity(0.72),
            borderRadius: BorderRadius.circular(8),
          ),
          child: Row(mainAxisSize: MainAxisSize.min, children: [
            Text(label, style: const TextStyle(color: Colors.white)),
            if (state == 'running' || state == 'countdown')
              btn(Icons.pause_rounded, 'Pause', m.pause),
            if (state == 'paused')
              btn(Icons.play_arrow_rounded, 'Resume', m.resume),
            if (active) btn(Icons.stop_rounded, 'Stop', m.stop),
            if (!active) btn(Icons.close_rounded, 'Close', m.dismiss),
          ]),
        ),
      );
    });
  }
}
