import fcntl,json,os,pathlib,pty,re,select,signal,struct,subprocess,sys,tempfile,termios,threading,time,unicodedata
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
binary=pathlib.Path(sys.argv[1]).resolve()

class Terminal:
 def __init__(self,width=120,height=36):self.width=width;self.height=height;self.raw=bytearray()
 def text(self):
  s=self.raw.decode('utf8','replace');rows=[[' ']*self.width for _ in range(self.height)];colors=[[None]*self.width for _ in range(self.height)];fg=None;x=y=i=0
  while i<len(s):
   c=s[i]
   if c=='\x1b':
    m=re.match(r'\x1b\[([0-?]*)([ -/]*)([@-~])',s[i:])
    if m:
     a=m.group(1);op=m.group(3);v=[int(q) if q else 0 for q in a.split(';')] if all(not q or q.isdigit() for q in a.split(';')) else [];n=v[0] if v and v[0] else 1
     if op in ('H','f'):y=max(0,n-1);x=max(0,(v[1] if len(v)>1 and v[1] else 1)-1)
     elif op=='A':y=max(0,y-n)
     elif op=='B':y=min(self.height-1,y+n)
     elif op=='C':x=min(self.width-1,x+n)
     elif op=='D':x=max(0,x-n)
     elif op=='G':x=max(0,n-1)
     elif op=='d':y=max(0,n-1)
     elif op=='m':
      at=0
      while at<len(v):
       code=v[at]
       if code in (0,39):fg=None
       elif code==38 and at+4<len(v) and v[at+1]==2:fg=tuple(v[at+2:at+5]);at+=4
       elif code in (38,48,58) and at+2<len(v) and v[at+1]==5:at+=2
       elif code in (48,58) and at+4<len(v) and v[at+1]==2:at+=4
       at+=1
     elif op=='J' and v and v[0]==2:rows=[[' ']*self.width for _ in range(self.height)];colors=[[None]*self.width for _ in range(self.height)]
     elif op=='K' and y<self.height:
      mode=v[0] if v else 0;lo,hi=(0,self.width) if mode==2 else ((0,min(self.width,x+1)) if mode==1 else (min(self.width,x),self.width));rows[y][lo:hi]=[' ']*(hi-lo)
     i+=len(m.group(0));continue
    m=re.match(r'\x1b\].*?(?:\x07|\x1b\\)',s[i:],re.S)
    if m:i+=len(m.group(0));continue
    i+=2;continue
   if c=='\r':x=0
   elif c=='\n':y=min(self.height-1,y+1)
   elif c>=' ':
    w=0 if unicodedata.combining(c) else 2 if unicodedata.east_asian_width(c) in 'WF' else 1
    if x<self.width and y<self.height:rows[y][x]=c;colors[y][x]=fg
    x+=w
   i+=1
  self.foregrounds=colors
  return '\n'.join(''.join(r).rstrip() for r in rows)

with tempfile.TemporaryDirectory(prefix='mh-ui-live-') as tmp:
 home=pathlib.Path(tmp);project=home/'project';project.mkdir();(project/'probe.txt').write_text('READ_FIXTURE_MARKER\n'+'// guard comment\n'*293);config=home/'.mh/config';config.mkdir(parents=True)
 clip=home/'clipboard.txt';helpers=home/'helpers';helpers.mkdir();fake=helpers/'powershell.exe'
 fake.write_text('#!/bin/bash\nlast="${@: -1}"\ncase "$last" in\n *GetImage*) exit 0;;\n *GetText*) /bin/cat '+str(clip)+';;\n *) /bin/cat > '+str(clip)+';;\nesac\n');fake.chmod(0o700)
 requests=[];main_requests=[];compaction_requests=[]
 def event(obj):return ('data: '+json.dumps(obj)+'\n\n').encode()
 class Server(BaseHTTPRequestHandler):
  protocol_version='HTTP/1.1'
  def log_message(self,*a):pass
  def do_POST(self):
   body=json.loads(self.rfile.read(int(self.headers['Content-Length'])));requests.append(body)
   assert body['store'] is False;assert body['model']=='wire';assert body['include']==['reasoning.encrypted_content']
   compact=body.get('instructions','').startswith('You are a context summarization assistant.')
   side=not body.get('tools') and not compact
   self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Connection','close');self.end_headers()
   def send(o):self.wfile.write(event(o));self.wfile.flush()
   if compact:
    compaction_requests.append(body);assert body['tool_choice']=='none'
    send({'type':'response.completed','response':{'output':[{'type':'message','id':'checkpoint','content':[{'type':'output_text','text':'## Original Request\nCOMPACT_FIXTURE_SUMMARY\n## Progress So Far\nRetain the controlled tool history.\n## Context Needed to Continue\nContinue fixture verification.','annotations':[]}]}],'usage':{'input_tokens':10,'output_tokens':2}}});return
   if side:
    assert body['tool_choice']=='none'
    second='SIDE_B_PENDING' in json.dumps(body['input'])
    if second:send({'type':'response.output_text.delta','output_index':0,'delta':'SIDE_B_PARTIAL'});time.sleep(2)
    send({'type':'response.completed','response':{'output':[{'type':'message','id':'side','content':[{'type':'output_text','text':'SIDE_B_ANSWER' if second else 'SIDE_ANSWER','annotations':[]}]}],'usage':{'input_tokens':2,'output_tokens':1}}});return
   main_requests.append(body);turn=len(main_requests)
   def fc(cid,name,args):
    item={'type':'function_call','id':'fc_'+cid,'call_id':cid,'name':name,'arguments':json.dumps(args)}
    send({'type':'response.output_item.done','output_index':len(output),'item':item});output.append(item)
   output=[{'type':'reasoning','id':'rs_'+str(turn),'summary':[],'encrypted_content':'opaque_'+str(turn)}]
   if turn==1:
    send({'type':'response.output_text.delta','delta':'STREAMING_MARKER\n'})
    for i in range(80):
     send({'type':'response.output_text.delta','delta':'.'});time.sleep(.06)
    fc('read','read',{'path':'probe.txt'})
   elif turn==2:
    assert body['reasoning']['effort']=='high'
    assert any(x.get('encrypted_content')=='opaque_1' for x in body['input'])
    assert any(x.get('type')=='function_call_output' and 'READ_FIXTURE_MARKER' in x.get('output','') for x in body['input'])
    assert 'STEER_REVISED' in json.dumps(body['input']) and 'STEER_ORIGINAL' not in json.dumps(body['input'])
    fc('question','ask_user_question',{'questions':[{'question':'Which fixture answer?','header':'Fixture','options':[{'label':'A','description':'First fixture'},{'label':'B','description':'Second fixture'}]}]})
    fc('todo','todo',{'action':'create','subject':'Fixture task'})
   else:
    assert any(x.get('type')=='function_call_output' and x.get('call_id')=='question' for x in body['input'])
    output.append({'type':'message','id':'final','phase':'final_answer','content':[{'type':'output_text','text':'FINAL_DONE','annotations':[]}]})
   send({'type':'response.completed','response':{'output':output,'usage':{'input_tokens':10,'output_tokens':2}}})
 server=ThreadingHTTPServer(('127.0.0.1',0),Server);server.daemon_threads=True;threading.Thread(target=server.serve_forever,daemon=True).start()
 base={'id':'wire','reasoning':True,'contextWindow':64000,'maxTokens':1024,'compaction':{'reserveTokens':2048,'keepRecentTokens':1000}}
 models=[dict(base,alias='fast',reasoningEffort='low'),dict(base,alias='deep',reasoningEffort='high')]
 (config/'config.json').write_text(json.dumps({'defaultModel':'fixture/fast','providers':{'fixture':{'api':'openai-responses-sse','base_url_env':'MH_PROBE_BASE','api_key_env':'MH_PROBE_KEY','models':models}}}))
 env={k:os.environ[k] for k in ('LANG','LC_ALL','WSL_INTEROP','WSL_DISTRO_NAME','DISPLAY','WAYLAND_DISPLAY','XDG_RUNTIME_DIR') if k in os.environ}
 env.update({'HOME':str(home),'USERPROFILE':str(home),'TERM':'xterm-256color','COLORTERM':'truecolor','PATH':str(helpers)+':/usr/bin:/bin','HTTP_PROXY':'','HTTPS_PROXY':'','NO_PROXY':'*','MH_PROBE_BASE':'http://127.0.0.1:'+str(server.server_port),'MH_PROBE_KEY':'fixture'})
 master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',36,120,0,0));original=termios.tcgetattr(slave);vt=Terminal()
 proc=subprocess.Popen([str(binary)],cwd=project,env=env,stdin=slave,stdout=slave,stderr=slave,start_new_session=True)
 def pump(seconds):
  end=time.monotonic()+seconds
  while time.monotonic()<end:
   ready,_,_=select.select([master],[],[],min(.05,max(0,end-time.monotonic())))
   if ready:
    try:b=os.read(master,65536)
    except OSError:break
    if not b:break
    vt.raw.extend(b)
    if b'\x1b[6n' in b:os.write(master,b'\x1b[1;1R')
   if proc.poll() is not None:break
 def keys(b,wait=.18):os.write(master,b);pump(wait)
 def until(text,timeout=8):
  end=time.monotonic()+timeout
  while time.monotonic()<end:
   if text in vt.text():return
   pump(.1)
  raise AssertionError('Missing '+text+'\n'+vt.text())
 def paste(s):keys(b'\x1b[200~'+s.encode()+b'\x1b[201~')
 try:
  pump(.6);assert proc.poll() is None,vt.text()
  assert 'thinking' not in vt.text().lower()
  keys(b'FIRST_INPUT\r',.5);until('STREAMING_MARKER')
  keys(b'STEER_ORIGINAL\r');keys(b'\x1bq');keys(b'\x15');keys(b'STEER_REVISED\r')
  keys(b'\x10');keys(b'/btw SIDE_INPUT\r');until('SIDE_ANSWER');keys(b'\x1b')
  until('Which fixture answer?',10)
  screen=vt.text();assert '┌' in screen and '╭' not in screen
  keys(b'\r');keys(b'\r');until('FINAL_DONE',8)
  # The native mouse/reporting path selects a real finalized transcript message.
  rows=vt.text().splitlines();y=next(i for i,r in enumerate(rows) if 'FINAL_DONE' in r);x=rows[y].index('FINAL_DONE')
  keys(('\x1b[<0;'+str(x+1)+';'+str(y+1)+'M').encode(),.05)
  keys(('\x1b[<32;'+str(x+10)+';'+str(y+1)+'M').encode(),.05)
  keys(('\x1b[<0;'+str(x+10)+';'+str(y+1)+'m').encode(),.05)
  keys(b'\x03',.4);assert clip.read_text()=='FINAL_DONE',clip.read_text();keys(b'\x1b')
  # Compare actual emitted RGB connector colors, not just a test buffer.
  keys(b'\x1b[1;5H');screen=vt.text();rows=screen.splitlines()
  start=next(y for y,row in enumerate(rows) if 'Read  probe.txt' in row)
  matches=[vt.foregrounds[y][2] for y in range(start+1,start+8) if rows[y].startswith(('  │ ','  └ '))]
  assert len(matches)==7 and all(color==(83,87,91) for color in matches),matches
  keys(b'\x1b[1;5F')
  # Modal model picker uses a plain-rule shared search and no thinking picker.
  keys(b'/model\r');until('Select model');assert 'Search' in vt.text();keys(b'\x1b')
  # Reopening an active side request must retain B, not restore completed A.
  keys(b'/btw SIDE_B_PENDING\r');until('SIDE_B_PARTIAL');keys(b'\x1b');keys(b'/btw\r')
  assert 'SIDE_B_PENDING' in vt.text() and 'SIDE_B_PARTIAL' in vt.text(),vt.text()
  until('SIDE_B_ANSWER');keys(b'\x1b')
  # Create a real local explicit-only skill without a model call.
  keys(b'/skills create\r');until('Create skill');keys(b'\r');paste('probe-manual');keys(b'\r');paste('Manual fixture skill');keys(b'\r');keys(b'\x1b[B');keys(b'\r');paste('Preserve SKILL_VERIFIED. Do not use tools.');keys(b'\r');keys(b'\r',.5)
  skill=project/'.mh/skills/probe-manual/SKILL.md';assert skill.exists(),vt.text();assert 'disable-model-invocation: true' in skill.read_text();assert 'SKILL_VERIFIED' in skill.read_text()
  # Ctrl+Z recovers input without suspending; Ctrl+R and former undo keys are inactive.
  keys(b'\x04');assert proc.poll() is None
  keys(b'draft alpha');keys(b'\x1f');keys(b'\x1bz');keys(b'\x12');assert 'draft alpha' in vt.text()
  keys(b'\x15');keys(b'\x1a');assert 'draft alpha' in vt.text() and proc.poll() is None
  keys(b'\x15')
  # Exercise the actual main-editor key routing, including enhanced Shift+Enter.
  paste('abcXYZ');keys(b'\x01');keys(b'\x06');keys(b'\x04');assert 'acXYZ' in vt.text()
  keys(b'\x05');keys(b'\x7f');assert 'acXY' in vt.text()
  keys(b'\x01');keys(b'\x06');keys(b'\x0b');keys(b'\x1a');assert 'acXY' in vt.text()
  keys(b'\x15');paste('line one');keys(b'\x1b[13;2u');paste('line two');keys(b'\x0a')
  assert 'line one' in vt.text() and 'line two' in vt.text() and len(main_requests)==3,vt.text()
  keys(b'\x03');keys(b'\x1a');assert 'line one' in vt.text() and 'line two' in vt.text()
  keys(b'\x03');pump(1.05)
  keys(b'/help\r');until('all shortcuts');keys(b'\x1b[F');until('Shared editor inside modal fields');keys(b'\x1b')
  keys(b'/compact preserve fixture\r');until('Context compacted')
  assert len(compaction_requests)==1,compaction_requests
  assert 'in 44' in vt.text() and 'out 10' in vt.text(),vt.text()
  fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',16,52,0,0));vt.width=52;vt.height=16;os.kill(proc.pid,signal.SIGWINCH);pump(.2)
  assert 'thinking' not in vt.text().lower()
  keys(b'/quit\r',.5);proc.wait(timeout=3);assert proc.returncode==0
  assert termios.tcgetattr(slave)==original
  raw=vt.raw.decode('utf8','replace');assert '🍀 mh' in raw and '\x1b[23;0t' in raw
  assert 'Resume this session with:' in raw
  assert len(main_requests)==3 and len(requests)==6,(len(main_requests),len(requests))
  journals=list((home/'.mh/sessions').rglob('session.jsonl'));assert len(journals)==1
  records=[json.loads(s) for s in journals[0].read_text().splitlines()]
  assert all('thinking' not in r['entry'].get('choice',{}) for r in records)
  assert not any('SIDE_INPUT' in json.dumps(r['entry']) or 'SIDE_ANSWER' in json.dumps(r['entry']) for r in records if r['entry']['type']=='message')
  final_prompt=[r['entry']['text'] for r in records if r['entry']['type']=='prompt'][-1];assert '<name>probe-manual</name>' not in final_prompt
  assert any(r['entry']['type']=='compaction' and 'COMPACT_FIXTURE_SUMMARY' in r['entry']['summary'] for r in records)
  usage=[]
  for record in records:
   entry=record['entry'];value=entry.get('usage')
   if entry['type']=='message' and entry['message']['role']=='assistant':value=entry['message']['response'].get('usage')
   if value:usage.append(value)
  assert sum(value['input'] for value in usage)==44 and sum(value['output'] for value in usage)==10
  # Launch the actual release again: durable usage/checkpoints survive, /btw does not.
  vt.raw=bytearray();proc=subprocess.Popen([str(binary),'--continue'],cwd=project,env=env,stdin=slave,stdout=slave,stderr=slave,start_new_session=True)
  pump(.6);until('Context compacted');assert 'in 44' in vt.text() and 'out 10' in vt.text(),vt.text()
  keys(b'/btw\r');until('No side-question history');assert len(requests)==6
  keys(b'/new\r');keys(b'/resume\r');until('Resume session');keys(b'\r');until('Context compacted')
  assert 'in 44' in vt.text() and 'out 10' in vt.text(),vt.text()
  keys(b'/exit\r',.5);proc.wait(timeout=3);assert proc.returncode==0 and termios.tcgetattr(slave)==original
  assert len(requests)==6
  # External termination must use the same shutdown/terminal restoration path.
  vt.raw=bytearray();proc=subprocess.Popen([str(binary),'--continue'],cwd=project,env=env,stdin=slave,stdout=slave,stderr=slave,start_new_session=True)
  pump(.6);assert proc.poll() is None
  os.kill(proc.pid,signal.SIGTERM);pump(.5);proc.wait(timeout=3)
  assert proc.returncode==0 and termios.tcgetattr(slave)==original
  print('PASS: actual release stream/Alt+Q re-edit/model-alias/tool/question/todo/btw/copy/skill-create/Ctrl+Z/RGB/help/compaction/usage/continue/new/resume/exit/resize/restoration; six loopback requests.')
 finally:
  if proc.poll() is None:os.killpg(proc.pid,signal.SIGTERM);proc.wait(timeout=3)
  os.close(master);os.close(slave);server.shutdown();server.server_close()
