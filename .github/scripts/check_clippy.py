"""Run strict Clippy and surface exact compiler diagnostics in CI annotations."""
import json
import subprocess
import sys

result = subprocess.run(
    ['cargo', 'clippy', '--locked', '--all-targets', '--all-features',
     '--message-format=json', '--', '-D', 'warnings'],
    stdout=subprocess.PIPE,
    text=True,
    check=False,
)
for line in result.stdout.splitlines():
    try:
        record = json.loads(line)
    except json.JSONDecodeError:
        print(line)
        continue
    if record.get('reason') != 'compiler-message':
        continue
    message = record['message']
    if message.get('rendered'):
        print(message['rendered'], end='')
    if message['level'] not in ('error', 'warning'):
        continue
    primary = next((span for span in message['spans'] if span['is_primary']), None)
    if primary:
        level = 'error' if message['level'] == 'error' else 'warning'
        text = message['message'].replace('%', '%25').replace('\r', '%0D').replace('\n', '%0A')
        print(f"::{level} file={primary['file_name']},line={primary['line_start']}::{text}")
sys.exit(result.returncode)
