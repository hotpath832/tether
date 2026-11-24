// Tether: Settings card that lets Claude (Claude Code / Claude Desktop) operate
// open remote sessions through MCP (see src/mcp_server.rs).
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:get/get.dart';

import '../../common.dart';
import '../../models/platform_model.dart';

class ClaudeControlSettings extends StatefulWidget {
  const ClaudeControlSettings({Key? key}) : super(key: key);

  @override
  State<ClaudeControlSettings> createState() => _ClaudeControlSettingsState();
}

class _ClaudeControlSettingsState extends State<ClaudeControlSettings> {
  static const _kOptWpm = 'tether-mcp-wpm';
  static const _kOptMistakes = 'tether-mcp-mistakes';
  Map<String, dynamic> _info = {};
  final _wpm = TextEditingController();
  final _mistakes = TextEditingController();

  @override
  void initState() {
    super.initState();
    _load();
    final w = bind.mainGetLocalOption(key: _kOptWpm);
    final m = bind.mainGetLocalOption(key: _kOptMistakes);
    _wpm.text = w.isEmpty ? '70' : w;
    _mistakes.text = m.isEmpty ? '2' : m;
  }

  @override
  void dispose() {
    _wpm.dispose();
    _mistakes.dispose();
    super.dispose();
  }

  void _load([String? json]) {
    try {
      _info = jsonDecode(json ?? bind.mainMcpInfo()) as Map<String, dynamic>;
    } catch (_) {
      _info = {};
    }
    if (mounted) setState(() {});
  }

  bool get _enabled => _info['enabled'] == true;
  String get _url => 'http://127.0.0.1:${_info['port'] ?? 21190}/mcp';
  String get _token => (_info['token'] ?? '').toString();

  String get _claudeCodeCmd =>
      'claude mcp add --transport http tether $_url --header "Authorization: Bearer $_token"';

  String get _claudeDesktopJson => const JsonEncoder.withIndent('  ').convert({
        'mcpServers': {
          'tether': {
            'command': (_info['exe'] ?? 'tether.exe').toString(),
            'args': ['--mcp'],
          }
        }
      });

  Widget _copyBox(String title, String value) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(translate(title),
                style: const TextStyle(fontWeight: FontWeight.w600))
            .marginOnly(top: 10, bottom: 4),
        Row(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Expanded(
            child: Container(
              padding: const EdgeInsets.all(8),
              decoration: BoxDecoration(
                border: Border.all(color: Theme.of(context).dividerColor),
                borderRadius: BorderRadius.circular(6),
              ),
              child: SelectableText(value,
                  style:
                      const TextStyle(fontFamily: 'monospace', fontSize: 12)),
            ),
          ),
          IconButton(
            tooltip: translate('Copy'),
            icon: const Icon(Icons.copy_rounded, size: 18),
            onPressed: () {
              Clipboard.setData(ClipboardData(text: value));
              showToast(translate('Copied'));
            },
          ),
        ]),
      ],
    );
  }

  void _saveTyping() {
    final w = double.tryParse(_wpm.text.trim());
    final m = double.tryParse(_mistakes.text.trim());
    if (w != null && w >= 5 && w <= 300) {
      bind.mainSetLocalOption(key: _kOptWpm, value: w.round().toString());
    }
    if (m != null && m >= 0 && m <= 30) {
      bind.mainSetLocalOption(key: _kOptMistakes, value: m.toString());
    }
  }

  Future<void> _newToken() async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (c) => AlertDialog(
        content: Text(translate(
            'Make a new token? Claude Code must then be set up again with the new command.')),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(c, false),
              child: Text(translate('Cancel'))),
          TextButton(
              onPressed: () => Navigator.pop(c, true),
              child: Text(translate('OK'))),
        ],
      ),
    );
    if (ok == true) _load(bind.mainMcpNewToken());
  }

  @override
  Widget build(BuildContext context) {
    final error = (_info['error'] ?? '').toString();
    final running = _info['running'] == true;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        CheckboxListTile(
          contentPadding: EdgeInsets.zero,
          dense: true,
          controlAffinity: ListTileControlAffinity.leading,
          title: Text(translate('Let Claude control open remote sessions')),
          value: _enabled,
          onChanged: (v) {
            if (v == null) return;
            _load(bind.mainMcpSetEnabled(enabled: v));
          },
        ),
        Text(translate('claude-control-tip'),
            style: Theme.of(context).textTheme.bodySmall),
        if (_enabled) ...[
          Text(
            running
                ? '${translate('Running at')} $_url'
                : '${translate('Not running')}${error.isNotEmpty ? ': $error' : ''}',
            style: TextStyle(
                color: running ? Colors.green : Colors.red, fontSize: 13),
          ).marginOnly(top: 8),
          _copyBox('Claude Code (run once in a terminal)', _claudeCodeCmd),
          _copyBox('Claude Desktop (add to claude_desktop_config.json)',
              _claudeDesktopJson),
          Row(children: [
            Expanded(
              child: TextField(
                controller: _wpm,
                decoration:
                    InputDecoration(labelText: translate('Typing speed (WPM)')),
                onChanged: (_) => _saveTyping(),
              ),
            ),
            const SizedBox(width: 12),
            Expanded(
              child: TextField(
                controller: _mistakes,
                decoration: InputDecoration(
                    labelText: translate('Typing mistakes (%)')),
                onChanged: (_) => _saveTyping(),
              ),
            ),
          ]).marginOnly(top: 8),
          Align(
            alignment: Alignment.centerLeft,
            child: TextButton(
              onPressed: _newToken,
              child: Text(translate('New token')),
            ),
          ).marginOnly(top: 4),
        ],
      ],
    ).marginOnly(left: 20, right: 8, bottom: 8);
  }
}
