"""Run native tests and expose bounded failure details in public CI annotations."""
import re
import subprocess
import sys

result = subprocess.run(['cargo', 'test', '--locked', *sys.argv[1:]],
                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                        text=True, encoding='utf-8', errors='replace', check=False)
print(result.stdout, end='')
if result.returncode:
    failures = re.findall(r"---- (.+?) stdout ----\n(.*?)(?=\n---- |\n\nfailures:|\Z)",
                          result.stdout, flags=re.DOTALL)
    for name, detail in failures:
        text = (name + ': ' + detail.strip())[:6000]
        text = text.replace('%', '%25').replace('\r', '%0D').replace('\n', '%0A')
        print(f'::error::{text}')
    if not failures:
        text = result.stdout[-6000:].replace('%', '%25').replace('\r', '%0D').replace('\n', '%0A')
        print(f'::error::Native test command failed: {text}')
sys.exit(result.returncode)
