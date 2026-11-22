"""Drive actual print-mode sessions through automatic compaction using loopback SSE."""
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

binary = str(pathlib.Path(sys.argv[1]).resolve())
requests = []
errors = []

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            requests.append(body)
            assert body['store'] is False
            compact = body['instructions'].startswith('You are a context summarization assistant.')
            if compact:
                assert not body.get('tools') and body['tool_choice'] == 'none'
                text = '## Goal\nAUTO_CHECKPOINT\n## Progress\nControlled prior turn summarized.'
                usage = {'input_tokens': 100, 'output_tokens': 20}
            else:
                text = 'AUTO_MAIN_ANSWER ' + 'retained response ' * 60
                usage = {'input_tokens': 7000, 'output_tokens': 100}
            response = {'type': 'response.completed', 'response': {
                'output': [{'type': 'message', 'id': 'fixture', 'content': [
                    {'type': 'output_text', 'text': text, 'annotations': []}]}], 'usage': usage}}
            encoded = ('data: ' + json.dumps(response) + '\n\n').encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
        except Exception as error:
            errors.append(type(error).__name__)
            raise

server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
server.daemon_threads = True
threading.Thread(target=server.serve_forever, daemon=True).start()
try:
    with tempfile.TemporaryDirectory(prefix='mh-auto-compaction-') as directory:
        home = pathlib.Path(directory)
        cwd = home / 'project'
        cwd.mkdir()
        config = home / '.mh/config'
        config.mkdir(parents=True)
        (config / 'config.json').write_text(json.dumps({'defaultModel': 'fixture/model', 'providers': {
            'fixture': {'api': 'openai-responses-sse', 'base_url_env': 'MH_PROBE_BASE', 'api_key_env': 'MH_PROBE_KEY',
                        'models': [{'id': 'model', 'contextWindow': 8192, 'maxTokens': 1024,
                                    'compaction': {'reserveTokens': 2048, 'keepRecentTokens': 500}}]}}}))
        env = {'HOME': str(home), 'USERPROFILE': str(home), 'PATH': '/usr/bin:/bin',
               'MH_PROBE_BASE': 'http://127.0.0.1:' + str(server.server_port), 'MH_PROBE_KEY': 'fixture',
               'HTTP_PROXY': '', 'HTTPS_PROXY': '', 'NO_PROXY': '*'}
        for number in range(2):
            command = [binary, '--print']
            if number:
                command.append('--continue')
            command.append('AUTO_USER_' + str(number) + ' ' + 'fixture input ' * 220)
            result = subprocess.run(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=15)
            assert result.returncode == 0, result.stderr.decode(errors='replace')
            assert b'AUTO_MAIN_ANSWER' in result.stdout
        assert not errors
        compact = [body for body in requests if body['instructions'].startswith('You are a context summarization assistant.')]
        assert compact, 'Automatic compaction must actually contact the fixture summarizer'
        main = [body for body in requests if body.get('tools')]
        assert len(main) == 2
        assert 'AUTO_CHECKPOINT' in json.dumps(main[-1]['input'])
        journals = list((home / '.mh/sessions').rglob('session.jsonl'))
        assert len(journals) == 1
        records = [json.loads(line)['entry'] for line in journals[0].read_text().splitlines()]
        assert any(entry['type'] == 'compaction' and 'AUTO_CHECKPOINT' in entry['summary'] for entry in records)
        assert any(entry['type'] == 'usage' and entry['kind'] == 'compaction_attempt' for entry in records)
        print('PASS actual mh --print/--continue automatic compaction, checkpoint replay, journal usage, and completed answers; no tools executed.')
finally:
    server.shutdown()
    server.server_close()
