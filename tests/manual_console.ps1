param([Parameter(Mandatory=$true)][string]$Binary,[switch]$SkipResize)
$ErrorActionPreference='Stop'
# Run only with the newly built native Windows executable. Synthetic provider,
# temporary home/workspace, no real credentials, no clipboard overwrite.
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
public sealed class MhFixtureServer : IDisposable {
    private readonly TcpListener listener;
    private readonly Thread thread;
    private readonly string[] replies;
    private volatile bool stopping;
    public int Requests;
    public string Error;
    public int Port { get { return ((IPEndPoint)listener.LocalEndpoint).Port; } }
    public MhFixtureServer(string[] responses) {
        replies=responses; listener=new TcpListener(IPAddress.Loopback,0); listener.Start();
        thread=new Thread(Run); thread.IsBackground=true; thread.Start();
    }
    private void Run() {
        try {
            while(!stopping) {
                using(var client=listener.AcceptTcpClient()) {
                    client.ReceiveTimeout=5000; client.SendTimeout=5000;
                    var stream=client.GetStream(); var header=new List<byte>();
                    while(true) {
                        int b=stream.ReadByte(); if(b<0) throw new IOException("request ended");
                        header.Add((byte)b); if(header.Count>65536) throw new IOException("header limit");
                        int n=header.Count;
                        if(n>=4&&header[n-4]==13&&header[n-3]==10&&header[n-2]==13&&header[n-1]==10) break;
                    }
                    string h=Encoding.ASCII.GetString(header.ToArray()); int length=0;
                    foreach(string line in h.Split(new[]{"\r\n"},StringSplitOptions.None))
                        if(line.StartsWith("content-length:",StringComparison.OrdinalIgnoreCase)) length=int.Parse(line.Substring(15).Trim());
                    if(length<0||length>1024*1024) throw new IOException("body limit");
                    byte[] body=new byte[length]; int at=0;
                    while(at<length){int n=stream.Read(body,at,length-at); if(n<=0)throw new IOException("body ended"); at+=n;}
                    string request=Encoding.UTF8.GetString(body);
                    if(!request.Contains("\"store\":false")) throw new IOException("stateless request missing");
                    int turn=Requests++;
                    if(turn==1&&!request.Contains("function_call_output")) throw new IOException("question result missing");
                    byte[] response=Encoding.UTF8.GetBytes(replies[Math.Min(turn,replies.Length-1)]);
                    byte[] status=Encoding.ASCII.GetBytes("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: "+response.Length+"\r\nConnection: close\r\n\r\n");
                    stream.Write(status,0,status.Length);stream.Write(response,0,response.Length);stream.Flush();
                }
            }
        } catch(Exception e){if(!stopping)Error=e.Message;}
    }
    public void Dispose(){stopping=true;listener.Stop();thread.Join(2000);}
}
public sealed class MhNativeConsole : IDisposable {
    [StructLayout(LayoutKind.Sequential)] public struct COORD { public short X,Y; public COORD(short x,short y){X=x;Y=y;} }
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public short Left,Top,Right,Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct INFO { public COORD Size,Cursor;public ushort Attributes;public RECT Window;public COORD Maximum; }
    [StructLayout(LayoutKind.Sequential,CharSet=CharSet.Unicode)] struct STARTUP {public int Size;public string Reserved,Desktop,Title;public uint X,Y,XSize,YSize,XChars,YChars,Fill,Flags;public ushort Show,ReservedSize;public IntPtr ReservedPtr,Input,Output,Error;}
    [StructLayout(LayoutKind.Sequential)] struct PROCESS {public IntPtr Handle,Thread;public uint Id,ThreadId;}
    [StructLayout(LayoutKind.Sequential,CharSet=CharSet.Unicode)] public struct KEY { [MarshalAs(UnmanagedType.Bool)] public bool Down;public ushort Repeat,VirtualKey,Scan;public char Unicode;public uint Control; }
    [StructLayout(LayoutKind.Explicit,CharSet=CharSet.Unicode,Size=20)] public struct RECORD { [FieldOffset(0)]public ushort Type;[FieldOffset(4)]public KEY Key;[FieldOffset(4)]public COORD Size; }
    [DllImport("kernel32",SetLastError=true,CharSet=CharSet.Unicode)] static extern bool CreateProcessW(string app,StringBuilder command,IntPtr pa,IntPtr ta,bool inherit,uint flags,IntPtr env,string cwd,ref STARTUP start,out PROCESS process);
    [DllImport("kernel32",SetLastError=true)] static extern bool AllocConsole();
    [DllImport("kernel32")] static extern bool FreeConsole();
    [DllImport("kernel32",SetLastError=true,CharSet=CharSet.Unicode)] static extern IntPtr CreateFileW(string name,uint access,uint share,IntPtr security,uint disposition,uint flags,IntPtr template);
    [DllImport("kernel32",SetLastError=true)] static extern bool GetConsoleMode(IntPtr handle,out uint mode);
    [DllImport("kernel32",SetLastError=true)] static extern bool SetConsoleMode(IntPtr handle,uint mode);
    [DllImport("kernel32",SetLastError=true)] static extern bool GetConsoleScreenBufferInfo(IntPtr handle,out INFO info);
    [DllImport("kernel32",SetLastError=true)] static extern bool SetConsoleScreenBufferSize(IntPtr handle,COORD size);
    [DllImport("kernel32",SetLastError=true)] static extern bool SetConsoleWindowInfo(IntPtr handle,bool absolute,ref RECT rect);
    [DllImport("kernel32",SetLastError=true)] static extern bool SetConsoleCursorPosition(IntPtr handle,COORD position);
    [DllImport("kernel32",SetLastError=true,CharSet=CharSet.Unicode)] static extern bool ReadConsoleOutputCharacterW(IntPtr handle,[Out]char[] text,uint length,COORD start,out uint read);
    [DllImport("kernel32",SetLastError=true,CharSet=CharSet.Unicode)] static extern bool WriteConsoleInputW(IntPtr handle,RECORD[] records,uint count,out uint written);
    [DllImport("kernel32")] static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32")] static extern uint WaitForSingleObject(IntPtr handle,uint timeout);
    [DllImport("kernel32")] static extern bool GetExitCodeProcess(IntPtr handle,out uint code);
    [DllImport("kernel32")] static extern bool TerminateProcess(IntPtr handle,uint code);
    [DllImport("kernel32")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32",SetLastError=true)] static extern bool SetHandleInformation(IntPtr handle,uint mask,uint flags);
    [DllImport("kernel32")] static extern IntPtr GetStdHandle(int type);
    [DllImport("kernel32")] static extern bool SetStdHandle(int type,IntPtr value);
    [DllImport("kernel32",CharSet=CharSet.Unicode)] static extern uint GetConsoleTitleW(StringBuilder title,uint size);
    private PROCESS process; private IntPtr input,output,oldIn,oldOut,oldErr;private uint initialInput;private bool disposed;
    private static void Check(bool ok,string operation){if(!ok)throw new IOException(operation+" failed ("+Marshal.GetLastWin32Error()+")");}
    public MhNativeConsole(string exe,string cwd,Dictionary<string,string> environment) {
        oldIn=GetStdHandle(-10);oldOut=GetStdHandle(-11);oldErr=GetStdHandle(-12);
        var keys=new List<string>(environment.Keys);keys.Sort(StringComparer.OrdinalIgnoreCase);var block=new StringBuilder();
        foreach(string key in keys)block.Append(key).Append('=').Append(environment[key]).Append('\0');block.Append('\0');
        IntPtr env=Marshal.StringToHGlobalUni(block.ToString());var start=new STARTUP();start.Size=Marshal.SizeOf(typeof(STARTUP));start.Flags=1;start.Show=0;
        try {
            // Allocate the fixture console before launching mh. A suspended new
            // process has not initialized its console, so AttachConsole races.
            FreeConsole();Check(AllocConsole(),"allocate owned console");
            input=CreateFileW("CONIN$",0xC0000000,3,IntPtr.Zero,3,0,IntPtr.Zero);output=CreateFileW("CONOUT$",0xC0000000,3,IntPtr.Zero,3,0,IntPtr.Zero);
            Check(input!=new IntPtr(-1)&&output!=new IntPtr(-1),"open console handles");Check(GetConsoleMode(input,out initialInput),"save input mode");
            uint outMode;Check(GetConsoleMode(output,out outMode),"read output mode");Check(SetConsoleMode(output,outMode|4),"enable console VT");Resize(120,36,false);
            Check(SetHandleInformation(input,1,1)&&SetHandleInformation(output,1,1),"inherit owned console handles");
            start.Flags|=0x100;start.Input=input;start.Output=output;start.Error=output;
            Check(CreateProcessW(exe,new StringBuilder("\""+exe+"\""),IntPtr.Zero,IntPtr.Zero,true,0x4|0x400,env,cwd,ref start,out process),"create owned fixture process");
            ResumeThread(process.Thread);
        } catch { Dispose(); throw; }
        finally{Marshal.FreeHGlobal(env);}
    }
    private void RefreshOutput(){IntPtr active=CreateFileW("CONOUT$",0xC0000000,3,IntPtr.Zero,3,0,IntPtr.Zero);Check(active!=new IntPtr(-1),"open active screen");if(output!=IntPtr.Zero)CloseHandle(output);output=active;}
    public void Resize(short width,short height,bool verify=true){RefreshOutput();INFO info;Check(GetConsoleScreenBufferInfo(output,out info),"read viewport");Check(SetConsoleScreenBufferSize(output,new COORD(Math.Max(width,info.Size.X),Math.Max(height,info.Size.Y))),"grow buffer");Check(SetConsoleCursorPosition(output,new COORD((short)Math.Min(info.Cursor.X,width-1),(short)Math.Min(info.Cursor.Y,height-1))),"clip fixture cursor");RECT rect=new RECT{Left=0,Top=0,Right=(short)(width-1),Bottom=(short)(height-1)};Check(SetConsoleWindowInfo(output,true,ref rect),"resize window");Check(GetConsoleScreenBufferInfo(output,out info),"read resized viewport");if(verify)Check(info.Window.Right-info.Window.Left+1==width&&info.Window.Bottom-info.Window.Top+1==height,"verify viewport size");uint written;RECORD resize=new RECORD{Type=4,Size=new COORD(width,height)};Check(WriteConsoleInputW(input,new[]{resize},1,out written)&&written==1,"notify viewport resize");}
    public string Capture(){RefreshOutput();INFO info;Check(GetConsoleScreenBufferInfo(output,out info),"capture viewport");int width=info.Window.Right-info.Window.Left+1,height=info.Window.Bottom-info.Window.Top+1;var result=new StringBuilder();for(int y=0;y<height;y++){char[] row=new char[width];uint read;Check(ReadConsoleOutputCharacterW(output,row,(uint)width,new COORD(info.Window.Left,(short)(info.Window.Top+y)),out read),"read bounded row");result.Append(row,0,(int)read).Append('\n');}return result.ToString();}
    public string WaitText(string text,int milliseconds=8000){var watch=Stopwatch.StartNew();while(watch.ElapsedMilliseconds<milliseconds){string screen=Capture();if(screen.Contains(text))return screen;if(WaitForSingleObject(process.Handle,0)==0)throw new IOException("fixture exited early");Thread.Sleep(50);}throw new IOException("fixture text timeout: "+text);}
    public void Key(ushort virtualKey,char unicode,uint modifiers=0){uint written;RECORD down=new RECORD{Type=1,Key=new KEY{Down=true,Repeat=1,VirtualKey=virtualKey,Unicode=unicode,Control=modifiers}};RECORD up=down;up.Key.Down=false;Check(WriteConsoleInputW(input,new[]{down,up},2,out written)&&written==2,"write fixture key press/release");}
    public void Text(string text){foreach(char c in text)Key(c=='\r'?(ushort)13:(ushort)0,c);}
    public string Title(){var title=new StringBuilder(512);GetConsoleTitleW(title,512);return title.ToString();}
    public void Finish(){Check(WaitForSingleObject(process.Handle,8000)==0,"wait for owned fixture exit");uint code;Check(GetExitCodeProcess(process.Handle,out code)&&code==0,"normal fixture exit");uint mode;Check(GetConsoleMode(input,out mode)&&mode==initialInput,"restore exact input mode");if(!Capture().Contains("Resume this session with:"))throw new IOException("resume hint missing");}
    public void Dispose(){if(disposed)return;disposed=true;if(process.Handle!=IntPtr.Zero&&WaitForSingleObject(process.Handle,0)!=0){TerminateProcess(process.Handle,1);WaitForSingleObject(process.Handle,5000);}if(input!=IntPtr.Zero)CloseHandle(input);if(output!=IntPtr.Zero)CloseHandle(output);FreeConsole();SetStdHandle(-10,oldIn);SetStdHandle(-11,oldOut);SetStdHandle(-12,oldErr);if(process.Thread!=IntPtr.Zero)CloseHandle(process.Thread);if(process.Handle!=IntPtr.Zero)CloseHandle(process.Handle);}
}
'@
$root=Join-Path ([IO.Path]::GetTempPath()) ('mh-console-fixture-'+[Guid]::NewGuid().ToString('N'))
$project=Join-Path $root 'project';$config=Join-Path $root '.mh\config'
New-Item -ItemType Directory -Path $project,$config | Out-Null
$question=@{questions=@(@{question='Native fixture question?';header='Fixture';options=@(@{label='A';description='First fixture'},@{label='B';description='Second fixture'})})} | ConvertTo-Json -Depth 8 -Compress
$call=@{type='function_call';id='fc_native';call_id='call_native';name='ask_user_question';arguments=$question}
$first='data: '+(@{type='response.output_item.done';output_index=0;item=$call} | ConvertTo-Json -Depth 12 -Compress)+"`n`n"+'data: '+(@{type='response.completed';response=@{output=@($call);usage=@{input_tokens=10;output_tokens=2}}} | ConvertTo-Json -Depth 12 -Compress)+"`n`n"
$final='data: '+(@{type='response.completed';response=@{output=@(@{type='message';id='msg_native';phase='final_answer';content=@(@{type='output_text';text='WINDOWS_NATIVE_CONSOLE_PASS';annotations=@()})});usage=@{input_tokens=10;output_tokens=2}}} | ConvertTo-Json -Depth 12 -Compress)+"`n`n"
$server=[MhFixtureServer]::new([string[]]@($first,$final))
$settings=@{defaultModel='fixture/model';providers=@{fixture=@{api='openai-responses-sse';base_url_env='MH_PROBE_BASE';api_key_env='MH_PROBE_KEY';models=@(@{id='model';contextWindow=64000;maxTokens=1024;compaction=@{reserveTokens=2048;keepRecentTokens=4000}})}}}
$settings | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $config 'config.json') -Encoding utf8
$environment=[Collections.Generic.Dictionary[string,string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach($key in @('SystemRoot','WINDIR','PATH','TEMP','TMP','ProgramFiles','ProgramFiles(x86)')){ $value=[Environment]::GetEnvironmentVariable($key);if($value){$environment.Add($key,$value)} }
foreach($entry in @{HOME=$root;USERPROFILE=$root;TERM='xterm-256color';MH_PROBE_BASE=('http://127.0.0.1:'+$server.Port);MH_PROBE_KEY='fixture';HTTP_PROXY='';HTTPS_PROXY='';NO_PROXY='*'}.GetEnumerator()){$environment[$entry.Key]=[string]$entry.Value}
$console=$null
$primary=$null
$cleanup=$null
try {
    $console=[MhNativeConsole]::new((Resolve-Path -LiteralPath $Binary).Path,$project,$environment)
    $console.WaitText('mh  v') | Out-Null
    $console.Key(68,[char]4,8)
    $console.Text('RECOVER_FIXTURE')
    $console.Key(85,[char]21,8);$console.Key(90,[char]26,8)
    $console.WaitText('RECOVER_FIXTURE') | Out-Null
    $console.Key(85,[char]21,8)
    $console.Text("/help`r");$console.WaitText('all shortcuts') | Out-Null
    $console.Key(27,[char]27)
    $console.Text("WINDOWS_FIXTURE_USER`r")
    $screen=$console.WaitText('Native fixture question?')
    if(!$screen.Contains('┌') -or $screen.Contains('╭')){throw 'Native rectangular modal missing'}
    $console.Key(13,[char]13)
    $console.Key(77,[char]13,8)
    $console.WaitText('WINDOWS_NATIVE_CONSOLE_PASS') | Out-Null
    if(!$console.Title().StartsWith('🍀 mh')){throw 'Native clover title missing'}
    $console.Text("/skills create`r");$console.WaitText('Create skill') | Out-Null
    $console.Key(13,[char]13);$console.Text("native-fixture`r");$console.Text("Native local skill`r");$console.Key(40,[char]0);$console.Key(13,[char]13);$console.Text("SKILL_NATIVE_BODY`r");$console.Key(13,[char]13)
    $skill=Join-Path $project '.mh\skills\native-fixture\SKILL.md'
    $watch=[Diagnostics.Stopwatch]::StartNew();while(!(Test-Path -LiteralPath $skill) -and $watch.ElapsedMilliseconds -lt 5000){Start-Sleep -Milliseconds 50}
    if(!(Test-Path -LiteralPath $skill)){throw 'Native local skill publication missing'}
    $body=Get-Content -LiteralPath $skill -Raw;if(!$body.Contains('disable-model-invocation: true') -or !$body.Contains('SKILL_NATIVE_BODY')){throw 'Native skill content changed'}
    $resizeFailure=$null
    if(!$SkipResize){try{$console.Resize(60,18);Start-Sleep -Milliseconds 300}catch{$resizeFailure=$_}}
    $console.Text("/exit`r");$console.Finish()
    Write-Output 'PASS native console startup/question/skill-create/title/normal exit/exact input-mode restoration/resume hint.'
    if($resizeFailure){throw $resizeFailure}
    if($server.Error){throw ('Native fixture server failed: '+$server.Error)}
    if($server.Requests -ne 2){throw 'Unexpected native fixture request count'}
    if($SkipResize){Write-Output 'SKIP native console resize; driver/platform viewport limitation remains unverified.'}
    Write-Output 'PASS native console Ctrl+Z recovery/Ctrl+D no-exit/help/question Enter+Ctrl+M/rectangular opacity/skill-create/clover-title/exit/input-mode restoration/resume hint.'
} catch {
    $primary=$_
    # Only controlled fixture content is captured; never real configuration.
    if($console){try{Write-Output ('FIXTURE SCREEN: '+$console.Capture())}catch{Write-Output 'Fixture screen unavailable.'}}
} finally {
    try{if($console){$console.Dispose()}}catch{$cleanup=$_}
    try{$server.Dispose()}catch{if(!$cleanup){$cleanup=$_}}
    try{if(Test-Path -LiteralPath $root){Remove-Item -LiteralPath $root -Recurse -Force}}catch{if(!$cleanup){$cleanup=$_}}
}
if($primary){if($cleanup){Write-Output 'Fixture cleanup also failed; primary failure preserved.'};throw $primary}
if($cleanup){throw $cleanup}
