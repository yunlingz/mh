$ErrorActionPreference = 'Stop'
# Authored transport checks using installed shells only. This is not a native
# compiled mh check and does not install tools or modify execution policy.
function Run-Fixture([string]$Executable, [bool]$Legacy, [string]$Command, [bool]$Sta = $false) {
    $prefix = 'try { [Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); $OutputEncoding=[Console]::OutputEncoding } catch {}' + "`n"
    $suffix = "`n;`nif (`$?) { exit 0 }`nif ((`$LASTEXITCODE -eq 0) -and (`"`$(`$Error[0].FullyQualifiedErrorId)`" -like `"NativeCommandError*`")) { exit 0 }`nexit 1"
    if (!$Legacy -or $Command.EndsWith('`')) { $suffix = '' }
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($prefix + $Command + $suffix))
    $info = [Diagnostics.ProcessStartInfo]::new($Executable)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $info.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    foreach ($arg in @('-NoProfile','-NonInteractive','-OutputFormat','Text')) { $info.ArgumentList.Add($arg) }
    if ($Sta) { $info.ArgumentList.Add('-STA') }
    $info.ArgumentList.Add('-EncodedCommand'); $info.ArgumentList.Add($encoded)
    $process = [Diagnostics.Process]::Start($info)
    $out = $process.StandardOutput.ReadToEndAsync(); $err = $process.StandardError.ReadToEndAsync()
    if (!$process.WaitForExit(8000)) { $process.Kill($true); throw 'Fixture transport timeout' }
    $result = @{ Code=$process.ExitCode; Out=$out.GetAwaiter().GetResult(); Err=$err.GetAwaiter().GetResult() }
    $process.Dispose(); return $result
}
$before = Get-ExecutionPolicy -List | ConvertTo-Json -Compress
$shells = @()
$modern = Get-Command pwsh.exe -ErrorAction SilentlyContinue
if ($modern) { $shells += @{ Exe=$modern.Source; Legacy=$false; Label='PowerShell 7' } }
$legacy = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
if (Test-Path $legacy) { $shells += @{ Exe=$legacy; Legacy=$true; Label='Windows PowerShell' } }
if (!$shells.Count) { throw 'No installed PowerShell shell available' }
foreach ($shell in $shells) {
    $r=Run-Fixture $shell.Exe $shell.Legacy "Write-Output 'héllo € 世界'; Write-Output 'line two'"
    if ($r.Code -ne 0 -or !$r.Out.Contains('héllo € 世界') -or !$r.Out.Contains('line two')) { throw 'UTF-8/multiline fixture failed' }
    $r=Run-Fixture $shell.Exe $shell.Legacy 'exit 7'
    if ($r.Code -ne 7) { throw 'Explicit exit-code fixture failed' }
    $r=Run-Fixture $shell.Exe $shell.Legacy 'cmd /c "exit 7"'
    if ($r.Code -ne 1) { throw 'Native-failure fixture failed' }
    $r=Run-Fixture $shell.Exe $shell.Legacy 'cmd /c "echo progress 1>&2 & exit 0" 2>&1 | Select-Object -Last 3'
    if ($r.Code -ne 0) { throw 'Native-stderr success fixture failed' }
    foreach ($unfinished in @('Get-ChildItem |','Write-Output x `','Write-Output "unclosed')) {
        $r=Run-Fixture $shell.Exe $shell.Legacy ('Write-Output MH_MUST_NOT_RUN; ' + $unfinished)
        if ($r.Code -eq 0 -or $r.Out.Contains('MH_MUST_NOT_RUN')) { throw 'Malformed script executed a prefix' }
    }
    Write-Output ($shell.Label + ': PASS UTF-8/multiline/explicit and native exits/stderr/whole-script parsing')
}
# Never overwrite nonempty user clipboard data to obtain a test result.
$clip = @'
Add-Type -AssemblyName System.Windows.Forms
$old=[System.Windows.Forms.Clipboard]::GetDataObject()
if ($old -and $old.GetFormats().Count -gt 0) { Write-Output 'CLIPBOARD_SKIPPED_NONEMPTY'; exit 0 }
try {
    $text="MH_CLIPBOARD_FIXTURE 世界`nline two"
    [System.Windows.Forms.Clipboard]::SetText($text)
    if ([System.Windows.Forms.Clipboard]::GetText() -cne $text) { throw 'Clipboard round trip failed' }
    Write-Output 'CLIPBOARD_EMPTY_ROUNDTRIP_PASS'
} finally { [System.Windows.Forms.Clipboard]::Clear() }
'@
$r=Run-Fixture $shells[0].Exe $shells[0].Legacy $clip $true
if ($r.Code -ne 0) { throw 'Clipboard transport fixture failed' }
if ($r.Out.Contains('CLIPBOARD_SKIPPED_NONEMPTY')) { Write-Output 'Clipboard: skipped to preserve nonempty user data' }
else { Write-Output 'Clipboard: PASS empty-clipboard roundtrip/restoration' }
$after = Get-ExecutionPolicy -List | ConvertTo-Json -Compress
if ($before -cne $after) { throw 'Execution policy changed' }
Write-Output 'PASS existing-shell transport checks; execution policy unchanged; no compiled-mh/MSVC claim.'
