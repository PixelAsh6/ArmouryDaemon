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
    Check 'the real config was not touched' (-not (Test-Path (Join-Path $env:APPDATA 'ArmouryDaemon\config.json-test')))
}
finally {
    if ($daemon -and -not $daemon.HasExited) {
        $daemon | Stop-Process -Force -ErrorAction SilentlyContinue
    }
    Get-Process -Name timeout -ErrorAction SilentlyContinue | Where-Object { $_.StartTime -gt (Get-Date).AddMinutes(-5) } | Stop-Process -Force -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $configDir -ErrorAction SilentlyContinue
    Remove-Item Env:\ARMOURY_TEST_NS -ErrorAction SilentlyContinue
}

""
if ($script:failures -eq 0) {
    "ALL CHECKS PASSED"
    exit 0
}
"$($script:failures) CHECK(S) FAILED"
exit 1
