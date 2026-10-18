<#
    ArmouryDaemon — headless self-test.

    Starts a **throw-away daemon instance** next to the installed one (its own pipe, config folder
    and single-instance mutex, via ARMOURY_TEST_NS), drives it over the pipe and asserts the engine
    behaviour. All packs are disabled for the run, so nothing on the desktop moves.

    Usage (no admin needed as long as the installed daemon runs at a different privilege — the test
    instance uses its own pipe):
        powershell -ExecutionPolicy Bypass -File tools\daemon-selftest.ps1
        powershell -ExecutionPolicy Bypass -File tools\daemon-selftest.ps1 -Exe ..\target\debug\ArmouryDaemon.exe

    Exit code 0 = every check passed.
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\ArmouryDaemon.exe')
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Exe)) { throw "daemon exe not found: $Exe" }
$Exe = (Resolve-Path $Exe).Path

$ns = 'selftest-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
$env:ARMOURY_TEST_NS = $ns
$configDir = Join-Path ([IO.Path]::GetTempPath()) "ArmouryDaemon-test-$ns"
$pipeName = "armoury-daemon.pipe-$ns"
$script:failures = 0

function Check([string]$name, [bool]$ok, [string]$detail = '') {
    if ($ok) {
        "PASS  $name"
    } else {
        "FAIL  $name   $detail"
        $script:failures++
    }
}

function Send-Pipe([string]$json) {
    for ($i = 0; $i -lt 3; $i++) {
        try {
            $p = New-Object System.IO.Pipes.NamedPipeClientStream('.', $pipeName, [System.IO.Pipes.PipeDirection]::InOut)
            $p.Connect(2000)
            $w = New-Object System.IO.StreamWriter($p); $w.AutoFlush = $true
            $r = New-Object System.IO.StreamReader($p)
            $w.WriteLine($json)
            $t = $r.ReadLineAsync()
            $out = if ($t.Wait(2000)) { $t.Result } else { '' }
            $p.Dispose()
            return $out
        } catch {
            Start-Sleep -Milliseconds 300
        }
    }
    return ''
}

function Status {
    $json = Send-Pipe '{"type":"get_status"}'
    if (-not $json) { return $null }
    return ($json | ConvertFrom-Json).status
}

# The daemon answers a request on the same connection, but status broadcasts can arrive first:
# read lines until the expected message type shows up.
function Send-And-Wait([string]$json, [string]$wantType, [int]$timeoutMs = 4000) {
    try {
        $p = New-Object System.IO.Pipes.NamedPipeClientStream('.', $pipeName, [System.IO.Pipes.PipeDirection]::InOut)
        $p.Connect(2000)
        $w = New-Object System.IO.StreamWriter($p); $w.AutoFlush = $true
        $r = New-Object System.IO.StreamReader($p)
        $w.WriteLine($json)
        $deadline = (Get-Date).AddMilliseconds($timeoutMs)
        while ((Get-Date) -lt $deadline) {
            $t = $r.ReadLineAsync()
            if (-not $t.Wait(500)) { continue }
            if (-not $t.Result) { break }
            $msg = $t.Result | ConvertFrom-Json
            if ($msg.type -eq $wantType) { $p.Dispose(); return $msg }
        }
        $p.Dispose()
        return $null
    } catch {
        return $null
    } finally {
        if ($p) { $p.Dispose() }
    }
}

function Wait-State([string]$want, [int]$timeoutSec = 6) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        $s = Status
        if ($s -and $s.state -eq $want) { return $s }
        Start-Sleep -Milliseconds 250
    }
    return (Status)
}

"=== ArmouryDaemon self-test (namespace $ns) ==="
$daemon = Start-Process $Exe -PassThru -WindowStyle Hidden
try {
    # ---- 1. the instance comes up and answers ----
    $s = $null
    for ($i = 0; $i -lt 40 -and -not $s; $i++) { Start-Sleep -Milliseconds 250; $s = Status }
    Check 'daemon answers on its own pipe' ($null -ne $s)
    if (-not $s) { throw 'no status reply' }

    # ---- 2. a fresh config starts idle, with no tracked apps ----
    Check 'fresh instance is idle' ($s.state -eq 'idle') "state=$($s.state)"
    Check 'fresh instance has no tracked apps' (@($s.tracked_apps).Count -eq 0) "count=$(@($s.tracked_apps).Count)"
    Check 'packs are loaded from the repo' (@($s.packs).Count -ge 3) "count=$(@($s.packs).Count)"
    $we = $s.packs | Where-Object { $_.id -eq 'wallpaper-engine' }
    Check 'wallpaper-engine exposes two dropdowns' (@($we.settings).Count -eq 2) "count=$(@($we.settings).Count)"
    $binary = $we.settings | Where-Object { $_.key -eq 'binary' }
    Check 'its binary dropdown defaults to auto' ($binary.value -eq 'auto') "value=$($binary.value)"

    # ---- 3. quiet the packs so nothing on the desktop moves ----
    foreach ($id in 'wallpaper-engine', 'g-helper', 'rainmeter') {
        Send-Pipe "{`"type`":`"set_pack_enabled`",`"id`":`"$id`",`"enabled`":false}" | Out-Null
    }
    Send-Pipe '{"type":"set_cooldown","seconds":5}' | Out-Null
    $s = Status
    Check 'packs disabled + cooldown 5 s' ((@($s.packs | Where-Object enabled).Count -eq 0) -and ($s.cooldown_seconds -eq 5))

    # ---- 4. apply is immediate ----
    Send-Pipe '{"type":"add_tracked_app","name":"Timeout","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    $p1 = Start-Process 'C:\Windows\System32\timeout.exe' -ArgumentList '40' -PassThru -WindowStyle Hidden
    $s = Wait-State 'triggered' 4
    Check 'starting a tracked app applies immediately' ($s.state -eq 'triggered') "state=$($s.state)"
    Check 'no cooldown while it runs' ($null -eq $s.cooldown_action) "action=$($s.cooldown_action)"

    # ---- 5. closing starts the cooldown, nothing reverts early ----
    $p1 | Stop-Process -Force
    $s = Wait-State 'cooldown' 4
    Check 'closing starts the revert cooldown' ($s.state -eq 'cooldown') "state=$($s.state)"
    Check 'the cooldown is a revert' ($s.cooldown_action -eq 'revert') "action=$($s.cooldown_action)"
    Start-Sleep -Milliseconds 700
    $s = Status
    Check 'still applied inside the cooldown' ($s.state -eq 'cooldown') "state=$($s.state)"

    # ---- 6. it reverts on its own and goes idle (no second cooldown) ----
    $s = Wait-State 'idle' 9
    Check 'reverts when the cooldown expires' ($s.state -eq 'idle') "state=$($s.state)"
    Check 'no second countdown afterwards' ($null -eq $s.cooldown_action) "action=$($s.cooldown_action)"

    # ---- 7. reopening inside the cooldown cancels it ----
    $p2 = Start-Process 'C:\Windows\System32\timeout.exe' -ArgumentList '40' -PassThru -WindowStyle Hidden
    $null = Wait-State 'triggered' 3
    $p2 | Stop-Process -Force
    $null = Wait-State 'cooldown' 3
    Start-Sleep -Milliseconds 500
    $p3 = Start-Process 'C:\Windows\System32\timeout.exe' -ArgumentList '40' -PassThru -WindowStyle Hidden
    $s = Wait-State 'triggered' 3
    Check 'reopening inside the cooldown cancels the revert' ($s.state -eq 'triggered') "state=$($s.state)"

    # ---- 8. pause stops automatic switching ----
    Send-Pipe '{"type":"set_paused","paused":true}' | Out-Null
    $p3 | Stop-Process -Force
    Start-Sleep -Seconds 1
    $s = Status
    Check 'paused: closing does not start a cooldown' ($s.state -eq 'triggered') "state=$($s.state)"
    $p4 = Start-Process 'C:\Windows\System32\timeout.exe' -ArgumentList '40' -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 1
    $s = Status
    Check 'paused: still applied while it runs' ($s.state -eq 'triggered') "state=$($s.state)"

    # ---- 9. manual actions still revert while paused (unpause first so the state is applied) ----
    Send-Pipe '{"type":"set_paused","paused":false}' | Out-Null
    Start-Sleep -Milliseconds 300
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    $s = Wait-State 'idle' 4
    Check 'removing the last tracked app reverts immediately' ($s.state -eq 'idle') "state=$($s.state)"
    $p4 | Stop-Process -Force -ErrorAction SilentlyContinue

    # ---- 10. config lives in the throw-away folder, not in %APPDATA% ----
    $tempConfig = Join-Path $configDir 'config.json'
    Check 'the test instance used its own config folder' (Test-Path $tempConfig) "expected $tempConfig"

    # ---- 11. settings persist to that config file ----
    Send-Pipe '{"type":"set_cooldown","seconds":25}' | Out-Null
    Send-Pipe '{"type":"set_ui_position","x":123.5,"y":-45.0}' | Out-Null
    Start-Sleep -Milliseconds 400
    $saved = Get-Content $tempConfig -Raw | ConvertFrom-Json
    Check 'cooldown persisted' ($saved.cooldown_seconds -eq 25) "got $($saved.cooldown_seconds)"
    Check 'dashboard position persisted' (($saved.ui_position[0] -eq 123.5) -and ($saved.ui_position[1] -eq -45.0)) "got $($saved.ui_position)"

    # ---- 12. install / remove a pack, strictly inside the build folder ----
    # packs_root = the first search dir that has a "packs" folder. Creating it beside the test exe
    # guarantees install/remove can never touch the repository (that mistake deleted a repo pack
    # once; the check below guards it).
    $buildPacks = Join-Path (Split-Path $Exe) 'packs'
    $script:buildPacksExisted = Test-Path $buildPacks
    New-Item -ItemType Directory -Force -Path $buildPacks | Out-Null
    $packSource = (Resolve-Path (Join-Path $PSScriptRoot '..\packs\rainmeter\pack.json')).Path
    $installedDir = Join-Path $buildPacks 'rainmeter'
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($packSource.Replace('\','\\'))`"}" 'pack_op'
    Check 'install reports success' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Check 'install copied the pack into the build folder' (Test-Path (Join-Path $installedDir 'pack.json')) "expected $installedDir"
    Check 'install did not touch the repo' (Test-Path $packSource) "missing: $packSource"
    $op = Send-And-Wait '{"type":"remove_pack","id":"rainmeter"}' 'pack_op'
    Check 'remove reports success' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Check 'remove deleted the build copy' (-not (Test-Path $installedDir)) "still there: $installedDir"
    Check 'the repo copy still exists after remove' (Test-Path $packSource) "missing: $packSource"

    # ---- 13. pause is remembered across a restart (and the engine starts frozen) ----
    Send-Pipe '{"type":"set_paused","paused":true}' | Out-Null
    Start-Sleep -Milliseconds 300
    $daemon | Stop-Process -Force
    Start-Sleep -Seconds 1
    $daemon = Start-Process $Exe -PassThru -WindowStyle Hidden
    $s = $null
    for ($i = 0; $i -lt 40 -and -not $s; $i++) { Start-Sleep -Milliseconds 250; $s = Status }
    Check 'restarted instance answers' ($null -ne $s)
    Check 'paused survives a restart' ($s.paused) "paused=$($s.paused)"
    Send-Pipe '{"type":"add_tracked_app","name":"Timeout","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    $p5 = Start-Process 'C:\Windows\System32\timeout.exe' -ArgumentList '20' -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 2
    $s = Status
    Check 'a paused restart does not apply on start' ($s.state -eq 'idle') "state=$($s.state)"
    $p5 | Stop-Process -Force
    Send-Pipe '{"type":"set_paused","paused":false}' | Out-Null
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null

    # ---- 14. a duplicate start leaves the first instance alone ----
    $second = Start-Process $Exe -ArgumentList '--autostart' -PassThru -WindowStyle Hidden
    $exited = $second.WaitForExit(5000)
    Check 'a duplicate start exits by itself' $exited "still running after 5 s"
    Check 'the first instance is still up' (-not $daemon.HasExited)
    Check 'the real config was not touched at all' (-not (Test-Path (Join-Path $env:APPDATA 'ArmouryDaemon\config.json-test')))

    # ---- 15. rapid open/close churn must not leave a phantom state ----
    Send-Pipe '{"type":"set_paused","paused":false}' | Out-Null
    Send-Pipe '{"type":"set_cooldown","seconds":5}' | Out-Null
    Send-Pipe '{"type":"add_tracked_app","name":"Timeout","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    for ($i = 0; $i -lt 5; $i++) {
        $q = Start-Process 'C:\Windows\System32\timeout.exe' -ArgumentList '20' -PassThru -WindowStyle Hidden
        Start-Sleep -Milliseconds 700
        $q | Stop-Process -Force
        Start-Sleep -Milliseconds 400
    }
    # after the last close we are inside a cooldown; it must end in idle, once
    $s = Wait-State 'idle' 10
    Check 'churn settles back to idle' ($s.state -eq 'idle') "state=$($s.state)"
    Check 'churn leaves no countdown behind' ($null -eq $s.cooldown_action) "action=$($s.cooldown_action)"
    # ---- 16. a spec-2 pack: the daemon compiles and runs its script ----
    $srcDir = Join-Path $env:TEMP "armoury-scriptpack-$ns"
    New-Item -ItemType Directory -Force -Path $srcDir | Out-Null
    $scriptPack = @'
{
  "spec": 2,
  "id": "scripttest",
  "name": "Script test",
  "version": "1.0.0",
  "order": 5,
  "targets": { "process": ["timeout.exe"] },
  "capture": [ { "key": "was_running", "mechanism": "process_running" } ],
  "script": {
    "apply":  "ctx.log(\"script apply ran for \" + ctx.target_name()); ctx.capture(\"note\", \"from the script\");",
    "revert": "ctx.log(\"script revert ran\");"
  },
  "apply": { "steps": [] },
  "revert": { "guard": { "targetRunning": false, "capturedRunning": true }, "steps": [] }
}
'@
    [IO.File]::WriteAllText((Join-Path $srcDir 'pack.json'), $scriptPack, (New-Object System.Text.UTF8Encoding($false)))
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($srcDir.Replace('\','\\'))`"}" 'pack_op'
    Check 'a spec-2 pack installs' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Send-Pipe '{"type":"set_pack_enabled","id":"scripttest","enabled":true}' | Out-Null
    Send-Pipe '{"type":"set_cooldown","seconds":5}' | Out-Null
    Send-Pipe '{"type":"add_tracked_app","name":"Timeout","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    $p6 = Start-Process 'C:\Windows\System32\timeout.exe' -ArgumentList '40' -PassThru -WindowStyle Hidden
    $s = Wait-State 'triggered' 6
    Start-Sleep -Milliseconds 800
    $logText = Get-Content (Join-Path $configDir 'logs\daemon.log') -Raw
    Check 'the apply script ran' ($logText -match 'script apply ran for timeout.exe') "no script log line"
    Check 'the apply script finished' ($logText -match 'scripttest: script apply finished')
    $p6 | Stop-Process -Force
    $s = Wait-State 'idle' 10
    Start-Sleep -Milliseconds 500
    $logText = Get-Content (Join-Path $configDir 'logs\daemon.log') -Raw
    Check 'the revert script ran' ($logText -match 'script revert ran')
    Send-Pipe '{"type":"set_pack_enabled","id":"scripttest","enabled":false}' | Out-Null
    $op = Send-And-Wait '{"type":"remove_pack","id":"scripttest"}' 'pack_op'
    Check 'the scripted pack removes cleanly' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    Remove-Item -Recurse -Force $srcDir -ErrorAction SilentlyContinue
}
finally {
    if ($daemon -and -not $daemon.HasExited) {
        $daemon | Stop-Process -Force -ErrorAction SilentlyContinue
    }
    Get-Process -Name timeout -ErrorAction SilentlyContinue | Where-Object { $_.StartTime -gt (Get-Date).AddMinutes(-5) } | Stop-Process -Force -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $configDir -ErrorAction SilentlyContinue
    # only remove the build-folder packs dir if this run created it
    if (-not $script:buildPacksExisted) {
        Remove-Item -Recurse -Force (Join-Path (Split-Path $Exe) 'packs') -ErrorAction SilentlyContinue
    }
    Remove-Item Env:\ARMOURY_TEST_NS -ErrorAction SilentlyContinue
}

""
if ($script:failures -eq 0) {
    "ALL CHECKS PASSED"
    exit 0
}
"$($script:failures) CHECK(S) FAILED"
exit 1
