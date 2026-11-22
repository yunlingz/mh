import json,os,pathlib,pty,select,subprocess,sys,tempfile,threading,time
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
binary=str(pathlib.Path(sys.argv[1]).resolve())
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  assert body['store'] is False
  assert any(item.get('role')=='user' and any(part.get('text')=='controlled context\ncontrolled fixture prompt' for part in item.get('content',[])) for item in body['input']), 'Piped context must be separated from the positional prompt'
  assert not any(t.get('name')=='ask_user_question' for t in body.get('tools',[]))
  output='before\x1b]52;c;ZmFrZQ==\x07after\nUnicode 日本\x1b]0;fake\x1b\\\x1b[2J'
  response={'type':'response.completed','response':{'output':[{'type':'message','id':'fixture','content':[{'type':'output_text','text':output,'annotations':[]}]}]}}
  data=('data: '+json.dumps(response)+'\n\n').encode()
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);server.daemon_threads=True;threading.Thread(target=server.serve_forever,daemon=True).start()
with tempfile.TemporaryDirectory(prefix='mh-print-tty-') as root:
 home=pathlib.Path(root);cwd=home/'project';cwd.mkdir();config=home/'.mh/config';config.mkdir(parents=True)
 (config/'config.json').write_text(json.dumps({'defaultModel':'fixture/model','providers':{'fixture':{'api':'openai-responses-sse','base_url_env':'MH_PROBE_BASE','api_key_env':'MH_PROBE_KEY','models':[{'id':'model','contextWindow':64000,'maxTokens':1024,'compaction':{'reserveTokens':2048,'keepRecentTokens':4000}}]}}}))
 env={'HOME':str(home),'USERPROFILE':str(home),'PATH':'/usr/bin:/bin','TERM':'xterm-256color','MH_PROBE_BASE':'http://127.0.0.1:'+str(server.server_port),'MH_PROBE_KEY':'fixture','HTTP_PROXY':'','HTTPS_PROXY':'','NO_PROXY':'*'}
 master,slave=pty.openpty();process=subprocess.Popen([binary,'--print','controlled fixture prompt'],cwd=cwd,env=env,stdin=subprocess.PIPE,stdout=slave,stderr=slave);os.close(slave)
 process.stdin.write(b'controlled context');process.stdin.close()
 raw=bytearray();end=time.monotonic()+8
 while time.monotonic()<end:
  ready,_,_=select.select([master],[],[],.1)
  if ready:
   try:data=os.read(master,65536)
   except OSError:break
   if not data:break
   raw.extend(data)
  if process.poll() is not None:
   try:raw.extend(os.read(master,65536))
   except OSError:pass
   break
 try: process.wait(timeout=6)
 except subprocess.TimeoutExpired:
  process.terminate();process.wait(timeout=3);raise
 finally: os.close(master)
 assert process.returncode==0
 assert b'\x1b' not in raw
 assert 'beforeafter'.encode() in raw and 'Unicode 日本'.encode() in raw
 print('PASS: actual --print TTY suppresses model OSC52/title/CSI controls; only safe captured PTY bytes; no desktop clipboard action.')
server.shutdown();server.server_close()
