<#
    ArmouryDaemon - headless self-test.

    Starts a **throw-away daemon instance** next to the installed one (its own pipe, config folder
    and single-instance mutex, via ARMOURY_TEST_NS), drives it over the pipe and asserts the engine
    behaviour. All packs are disabled for the run, so nothing on the desktop moves.

    Usage (run this from an elevated PowerShell - the executable asks Windows for administrator
    rights, so a shell that is not elevated skips the command-line probes and says why):
        powershell -ExecutionPolicy Bypass -File tools\daemon-selftest.ps1
        powershell -ExecutionPolicy Bypass -File tools\daemon-selftest.ps1 -Exe ..\target\debug\ArmouryDaemon.exe

    The exe has to live inside this repository. The daemon resolves its packs folder to the first
    search folder that already has one, and the install/remove checks write into that folder and
    delete the pack again - against a live install that would overwrite and then remove
    C:\Apps\ArmouryDaemon\packs\<id>. A run outside the repository is refused.

    Exit code 0 = every check passed.
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\ArmouryDaemon.exe')
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Exe)) { throw "daemon exe not found: $Exe" }
$Exe = (Resolve-Path $Exe).Path

# ---- refuse to run against a live install -----------------------------------

# The daemon picks its packs folder like this: the first search folder that already has a
# "packs" folder, else "packs" beside the exe (packs::packs_root). The install/remove checks
# below write into that folder and then delete the pack again, so the resolved root has to sit
# inside this repository. Anything else (C:\Apps\..., a copy next to a running daemon) is a live
# install and the run is refused before a single byte is written.
$exeDir = Split-Path $Exe -Parent
$searchDirs = @($exeDir)
$up2 = Split-Path (Split-Path $exeDir -Parent) -Parent
if ($up2) { $searchDirs += $up2 }
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))

function Get-PacksRoot([string[]]$dirs) {
    foreach ($d in $dirs) {
        $candidate = [IO.Path]::GetFullPath((Join-Path $d 'packs'))
        if (Test-Path -LiteralPath $candidate -PathType Container) { return $candidate }
    }
    return [IO.Path]::GetFullPath((Join-Path $dirs[0] 'packs'))
}

function Test-Under([string]$root, [string]$path) {
    return $path.StartsWith(($root.TrimEnd('\') + '\'), [System.StringComparison]::OrdinalIgnoreCase)
}

$packsRoot = Get-PacksRoot $searchDirs
if (-not (Test-Under $repoRoot $packsRoot)) {
    throw ("refusing to run: the daemon would use the packs folder '$packsRoot', which is outside the repository" +
        " ('$repoRoot'). Point -Exe at a build inside the repository (e.g. ..\target\release\ArmouryDaemon.exe)," +
        " never at an installed copy.")
}
# the scratch folder the harness installs into: beside the test exe, never the repository's own packs
$buildPacks = [IO.Path]::GetFullPath((Join-Path $exeDir 'packs'))
if (-not (Test-Under $repoRoot $buildPacks) -or $buildPacks -eq (Join-Path $repoRoot 'packs')) {
    throw "refusing to run: the harness would install into '$buildPacks', which is not a scratch folder of this repository."
}

$ns = 'selftest-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
$env:ARMOURY_TEST_NS = $ns
$configDir = Join-Path ([IO.Path]::GetTempPath()) "ArmouryDaemon-test-$ns"
$pipeName = "armoury-daemon.pipe-$ns"
# The command-line probes get a namespace of their own: a daemon that a typo would start is
# stopped at once by the single-instance mutex of the test instance, so the process count
# would never change and the check would prove nothing. In its own namespace such a daemon
# stays alive and is seen.
$cliNs = 'selftest-cli-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
$cliConfigDir = Join-Path ([IO.Path]::GetTempPath()) "ArmouryDaemon-test-$cliNs"
# where the captured output of the probes lands
$cliDir = Join-Path $configDir 'cli'
# The log of the throw-away instance. It is the only place where the harness can see that the
# packs really changed: the status reports the state, the log reports the change.
$logPath = Join-Path $configDir 'logs\daemon.log'
$script:failures = 0
$script:children = New-Object System.Collections.ArrayList
$script:buildPacksExisted = $true
$script:parkedPack = $null

function Check([string]$name, [bool]$ok, [string]$detail = '') {
    if ($ok) {
        "PASS  $name"
    } else {
        "FAIL  $name   $detail"
        $script:failures++
    }
}

# Start a process the harness keeps track of. The finally block kills exactly these - never
# every timeout.exe on the machine, which would take a build script or a terminal with it.
function Start-Tracked([string]$file, [string]$arguments) {
    $p = Start-Process $file -ArgumentList $arguments -PassThru -WindowStyle Hidden
    [void]$script:children.Add($p)
    return $p
}

# The SHA256 of a file, or '' when it does not exist.
function Get-FileSha([string]$path) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return '' }
    try { return (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash } catch { return 'unreadable' }
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

# The times at which the daemon really changed the packs: 'apply' for an apply, 'revert' for a
# revert. The log has a resolution of one second, which is enough for a cooldown of 5 s.
function Get-ChangeTimes([string]$what) {
    $times = @()
    if (-not (Test-Path -LiteralPath $logPath)) { return $times }
    $pattern = if ($what -eq 'apply') { 'packs applied \(triggered by' } else { 'packs reverted' }
    $lines = @()
    try { $lines = Get-Content -LiteralPath $logPath } catch { return $times }
    foreach ($line in $lines) {
        if ($line -notmatch $pattern) { continue }
        if ($line -match '^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})') {
            $times += [datetime]::ParseExact($Matches[1], 'yyyy-MM-dd HH:mm:ss', [System.Globalization.CultureInfo]::InvariantCulture)
        }
    }
    return $times
}

# Wait until the daemon log shows more changes of that kind than $count. Returns all of them.
function Wait-Change([string]$what, [int]$count, [int]$timeoutSec = 20) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        $times = @(Get-ChangeTimes $what)
        if ($times.Count -gt $count) { return $times }
        Start-Sleep -Milliseconds 200
    }
    return @(Get-ChangeTimes $what)
}

# The seconds between two moments. A moment that never happened counts as "never" (9999), so a
# check that needs a real distance fails instead of passing on a missing value.
function Get-Gap($from, $to) {
    if ($null -eq $from -or $null -eq $to) { return 9999 }
    return ($to - $from).TotalSeconds
}

# Wait until the daemon log carries a line matching the pattern, and hand back the last
# match, or $null when nothing matched (a $null in a check fails as "not there", never
# as an error in the plumbing).
function Wait-DaemonLogLine([string]$pattern, [int]$timeoutSec = 6) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    $found = $null
    while ((Get-Date) -lt $deadline) {
        $found = @(((Read-Text $logPath) -split "`r?`n") | Where-Object { $_ -match $pattern } | Select-Object -Last 1)
        if ($found) { return $found }
        Start-Sleep -Milliseconds 250
    }
    return $null
}

# The times at which a named pack logged that it applied ('applied') or reverted ('reverted').
# The line carries the pack's name, not its id: "Sluggish: reverted (target: n/a)".
function Get-PackLineTimes([string]$name, [string]$action) {
    $times = @()
    if (-not (Test-Path -LiteralPath $logPath)) { return $times }
    $pattern = '^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}).*' + [regex]::Escape($name) + ': ' + $action + ' \(target'
    $lines = @()
    try { $lines = Get-Content -LiteralPath $logPath } catch { return $times }
    foreach ($line in $lines) {
        if ($line -match $pattern) {
            $times += [datetime]::ParseExact($Matches[1], 'yyyy-MM-dd HH:mm:ss', [System.Globalization.CultureInfo]::InvariantCulture)
        }
    }
    return $times
}

# The content of a file, or '' when it is empty or missing. Get-Content hands back $null for
# an empty file, which would break every string operation on it.
function Read-Text([string]$path) {
    if (-not (Test-Path -LiteralPath $path)) { return '' }
    $text = Get-Content -LiteralPath $path -Raw
    if ($null -eq $text) { return '' }
    return [string]$text
}

# The number of live processes of this very exe. The installed daemon runs from another path
# and is never counted; a path that cannot be read any more belongs to a process that has
# already exited.
function Get-ExeProcessCount {
    $count = 0
    $name = [IO.Path]::GetFileNameWithoutExtension($Exe)
    foreach ($p in @(Get-Process -Name $name -ErrorAction SilentlyContinue)) {
        try { if ($p.Path -eq $Exe) { $count++ } } catch { }
    }
    return $count
}

# The first non-empty line of a captured stream, for a short failure detail.
function Get-FirstLine([string]$text) {
    foreach ($line in ($text -split "`r?`n")) {
        if ($line.Trim()) { return $line.Trim() }
    }
    return ''
}

# Is this shell elevated? The executable asks Windows for administrator rights (the manifest
# it carries), so a program it starts from a shell that is not elevated raises a UAC prompt.
# An admin user in a normal shell is NOT elevated: the administrator group is deny-only in
# the filtered token, and IsInRole says so.
function Test-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# Run the binary with these arguments and capture what it says.
#
# The release build is a windows-subsystem binary: it attaches to the console of the parent
# process and, when its standard output is not a console, it writes the line to stderr as
# well. The output is therefore redirected into two files, and both are read back.
#
# The process is not stopped here: a run that does not end on its own is a resident daemon,
# and the caller has to count it before it is killed.
function Invoke-Cli([string[]]$argList, [int]$timeoutSec = 20) {
    New-Item -ItemType Directory -Force -Path $cliDir | Out-Null
    $outPath = Join-Path $cliDir 'stdout.txt'
    $errPath = Join-Path $cliDir 'stderr.txt'
    Remove-Item -LiteralPath $outPath, $errPath -ErrorAction SilentlyContinue
    $p = Start-Process $Exe -ArgumentList $argList -PassThru -WindowStyle Hidden `
            -RedirectStandardOutput $outPath -RedirectStandardError $errPath
    # the finally block stops it, whatever it turns out to be
    [void]$script:children.Add($p)
    $null = $p.Handle    # cache the handle, or the exit code cannot be read any more
    $exited = $p.WaitForExit($timeoutSec * 1000)
    $code = $null
    if ($exited) { $code = $p.ExitCode }
    $out = Read-Text $outPath
    $err = Read-Text $errPath
    return [pscustomobject]@{
        Process = $p
        Exited  = $exited
        Exit    = $code
        Out     = $out
        Err     = $err
        Text    = "$out`n$err"
    }
}

# Run the binary with these arguments and watch what it leaves behind.
#
# The number of live processes of this very exe before and after the run says whether a
# daemon was started: the old command line started a resident daemon on a typo. A run that
# does not end on its own is such a daemon - it is stopped here, and the checks say so.
function Invoke-CliProbe([string[]]$argList, [int]$timeoutSec = 20) {
    $before = Get-ExeProcessCount
    $run = Invoke-Cli $argList $timeoutSec
    Start-Sleep -Milliseconds 500
    $after = Get-ExeProcessCount
    $killed = $false
    if (-not $run.Exited) {
        $run.Process | Stop-Process -Force -ErrorAction SilentlyContinue
        $killed = $true
    }
    $detail = "exit=$($run.Exit)"
    if ($killed) { $detail += ' (a stray daemon was killed)' }
    return [pscustomobject]@{
        Run    = $run
        Before = $before
        After  = $after
        Killed = $killed
        Detail = $detail
    }
}

# Run the binary with these arguments while a local server answers the request, then
# read back what it said.
#
# The order matters: the server has to answer while the binary waits for it, so the
# process is started first and the server is called from here. The server is a script
# block that gets the loopback port it has to answer on.
function Invoke-CatalogProbe([string[]]$argList, [scriptblock]$serve, [int]$timeoutSec = 40) {
    New-Item -ItemType Directory -Force -Path $cliDir | Out-Null
    $outPath = Join-Path $cliDir 'stdout.txt'
    $errPath = Join-Path $cliDir 'stderr.txt'
    Remove-Item -LiteralPath $outPath, $errPath -ErrorAction SilentlyContinue
    $p = Start-Process $Exe -ArgumentList $argList -PassThru -WindowStyle Hidden `
            -RedirectStandardOutput $outPath -RedirectStandardError $errPath
    [void]$script:children.Add($p)
    $null = $p.Handle    # cache the handle, or the exit code cannot be read any more
    # the local server answers while the binary waits for it
    try { & $serve } catch { "the local server failed: $_" }
    $exited = $p.WaitForExit($timeoutSec * 1000)
    $code = $null
    if ($exited) { $code = $p.ExitCode }
    if (-not $exited) { $p | Stop-Process -Force -ErrorAction SilentlyContinue }
    $out = Read-Text $outPath
    $err = Read-Text $errPath
    return [pscustomobject]@{
        Process = $p
        Exited  = $exited
        Exit    = $code
        Out     = $out
        Err     = $err
        Text    = "$out`n$err"
    }
}

# The request of one connection, as text. The client sends headers only, so the first
# blank line ends it.
function Read-Request([System.Net.Sockets.TcpClient]$client) {
    $stream = $client.GetStream()
    $buffer = New-Object byte[] 8192
    $text = ''
    while (-not $text.Contains("`r`n`r`n")) {
        $read = $stream.Read($buffer, 0, $buffer.Length)
        if ($read -le 0) { break }
        $text += [System.Text.Encoding]::ASCII.GetString($buffer, 0, $read)
        if ($text.Length -gt 65536) { break }
    }
    return $text
}

# The gzip-compressed bytes of a string.
function Get-GzipBytes([string]$text) {
    $raw = [System.Text.Encoding]::UTF8.GetBytes($text)
    $ms = [System.IO.MemoryStream]::new()
    $gz = [System.IO.Compression.GZipStream]::new($ms, [System.IO.Compression.CompressionMode]::Compress, $true)
    $gz.Write($raw, 0, $raw.Length)
    $gz.Dispose()
    return $ms.ToArray()
}

# A loopback listener on a free port, and the task that waits for the first connection.
function Start-LoopbackListener {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    return [pscustomobject]@{
        Listener = $listener
        Port     = $listener.LocalEndpoint.Port
        Accept   = $listener.AcceptTcpClientAsync()
    }
}

# Why a fetch failed: the TLS check refused the certificate, the network is not there, or
# something else. The first is the build, the second is the machine, the third is a bug.
function Get-FetchFailure([string]$text) {
    if ($text -match '(?i)certificate|unknownissuer|untrusted|rustls') { return 'tls' }
    if ($text -match '(?i)host not found|connection failed|timeout|dns|no such host') { return 'network' }
    # only the "no .minisig to fetch" case is the transitional one - a published
    # signature that does not VERIFY is a failure, and must be red
    if ($text -match '(?i)signature could not be fetched') { return 'signature' }
    return 'other'
}

# The real config of the installed daemon must not change during the run: the test instance
# writes into a temp folder (ARMOURY_TEST_NS), so the file has to be byte-identical afterwards.
$realConfig = Join-Path $env:APPDATA 'ArmouryDaemon\config.json'
$script:realConfigBefore = Get-FileSha $realConfig

"=== ArmouryDaemon self-test (namespace $ns) ==="
"=== packs root: $packsRoot | scratch: $buildPacks ==="
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

    # ---- 4. a tracked app start applies the packs and starts a lockout ----
    # The engine (2026-10-22): the lockout holds a change in *both* directions. This start
    # finds no window running - the machine is free, the first launch of a session - so the
    # packs come on at once, and the change starts a window of `cooldown` seconds. A start
    # *inside* a window waits for it (section 9c). A close waits for a window too, and a fast
    # restart inside that window cancels the wait. The daemon reports the window as the
    # "cooldown" state plus the time that is left.
    $applies = @(Get-ChangeTimes 'apply').Count
    Send-Pipe '{"type":"add_tracked_app","name":"Timeout","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    $p1 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    $s = Wait-State 'cooldown' 8
    Check 'a tracked app start applies the packs' (@(Get-ChangeTimes 'apply').Count -ge $applies + 1) "applies=$applies now=$(@(Get-ChangeTimes 'apply').Count)"
    Check 'the apply starts a lockout' ($s.state -eq 'cooldown') "state=$($s.state)"
    Check 'the lockout counts down' ($s.cooldown_remaining_ms -gt 0 -and $s.cooldown_remaining_ms -le 5000) "remaining=$($s.cooldown_remaining_ms)"
    Check 'the applied state names the app' ($s.trigger_source -eq 'timeout.exe') "source=$($s.trigger_source)"
    Check 'no change is waiting behind the window' ($null -eq $s.cooldown_action) "action=$($s.cooldown_action)"

    # ---- 5. the lockout ends while the app runs: nothing changes ----
    # A lockout that expires while the intent still holds changes nothing and starts no new one.
    $reverts = @(Get-ChangeTimes 'revert').Count
    $s = Wait-State 'triggered' 15
    Check 'the packs stay applied when the lockout expires' ($s.state -eq 'triggered') "state=$($s.state)"
    Check 'no lockout is left behind' ($null -eq $s.cooldown_remaining_ms) "remaining=$($s.cooldown_remaining_ms)"
    Check 'nothing was reverted' (@(Get-ChangeTimes 'revert').Count -eq $reverts) "reverts=$reverts now=$(@(Get-ChangeTimes 'revert').Count)"

    # ---- 6. a close with no lockout running holds the revert ----
    # The close always waits for a window: it starts one if none is running and keeps the
    # running one otherwise, and the revert lands when that window is over - so a fast restart
    # inside the window cancels it and the packs stay applied.
    $reverts = @(Get-ChangeTimes 'revert').Count
    $closeAt = Get-Date
    $p1 | Stop-Process -Force
    Start-Sleep -Milliseconds 700
    Check 'the close holds the revert' (@(Get-ChangeTimes 'revert').Count -eq $reverts) "reverts=$reverts now=$(@(Get-ChangeTimes 'revert').Count)"
    $times = @(Wait-Change 'revert' $reverts 15)
    Check 'the held revert lands when the lockout expires' ($times.Count -ge $reverts + 1) "reverts=$reverts now=$($times.Count)"
    $revertAt = $times[$times.Count - 1]
    $revertSeenAt = Get-Date
    Check 'the revert waits for the lockout' ((Get-Gap $closeAt $revertAt) -ge 3 -and (Get-Gap $closeAt $revertAt) -le 12) "close=$closeAt revert=$revertAt"
    $s = Wait-State 'cooldown' 8
    Check 'the revert starts a lockout of its own' ($s.state -eq 'cooldown') "state=$($s.state)"

    # ---- 7. that lockout runs before the daemon goes idle ----
    # The change that just happened starts a lockout too: the daemon stays "cooldown" for one
    # more period and only then goes "idle".
    $s = Wait-State 'idle' 20
    $idleSeenAt = Get-Date
    Check 'the daemon goes idle after the lockout' ($s.state -eq 'idle') "state=$($s.state)"
    Check 'the revert is followed by a lockout of its own' ((Get-Gap $revertSeenAt $idleSeenAt) -ge 3.5 -and (Get-Gap $revertSeenAt $idleSeenAt) -le 12) "gap=$(Get-Gap $revertSeenAt $idleSeenAt)s"

    # ---- 8. a close inside a lockout is deferred, not dropped ----
    # The app starts again (that start starts a lockout) and is closed while the lockout still
    # runs. The close may not touch the packs: it is remembered and applied when the lockout
    # ends.
    $applies = @(Get-ChangeTimes 'apply').Count
    $reverts = @(Get-ChangeTimes 'revert').Count
    $p2 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    $s = Wait-State 'cooldown' 8
    $applyTimes = @(Wait-Change 'apply' $applies 15)
    $applyAt = $applyTimes[$applyTimes.Count - 1]
    Check 'the new start applies the packs again' ($applyTimes.Count -ge $applies + 1) "applies=$applies now=$($applyTimes.Count)"
    $s = Status
    $left = $s.cooldown_remaining_ms
    # the close has to go out while the lockout still runs, with room for the watcher
    Check 'the close goes out inside the lockout' ($left -gt 2000) "remaining=$left"
    $p2 | Stop-Process -Force
    Start-Sleep -Milliseconds 1000
    $s = Status
    Check 'the close inside the lockout is deferred' (@(Get-ChangeTimes 'revert').Count -eq $reverts) "reverts=$reverts now=$(@(Get-ChangeTimes 'revert').Count)"
    Check 'the state is still the lockout' ($s.state -eq 'cooldown') "state=$($s.state)"
    Check 'the lockout keeps running, it was not restarted' ($s.cooldown_remaining_ms -lt $left) "before=$left now=$($s.cooldown_remaining_ms)"
    $times = @(Wait-Change 'revert' $reverts 20)
    Check 'the deferred revert happens when the lockout expires' ($times.Count -ge $reverts + 1) "reverts=$reverts now=$($times.Count)"
    $revertAt = $times[$times.Count - 1]
    Check 'the deferred revert waits for the lockout' ((Get-Gap $applyAt $revertAt) -ge 3 -and (Get-Gap $applyAt $revertAt) -le 12) "apply=$applyAt revert=$revertAt"
    $s = Wait-State 'idle' 20
    Check 'the deferred revert leaves the daemon idle' ($s.state -eq 'idle') "state=$($s.state)"

    # ---- 9. a fast restart inside the lockout never reverts ----
    # The app is closed and opened again while the lockout of the start still runs: the intent
    # holds again, so nothing is changed and nothing is reverted when the lockout ends.
    $applies = @(Get-ChangeTimes 'apply').Count
    $reverts = @(Get-ChangeTimes 'revert').Count
    $p3 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    $s = Wait-State 'cooldown' 8
    $null = Wait-Change 'apply' $applies 15
    $applies = @(Get-ChangeTimes 'apply').Count
    $p3 | Stop-Process -Force
    Start-Sleep -Milliseconds 700
    $p4 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    $s = Status
    $left = $s.cooldown_remaining_ms
    Check 'the restart goes out inside the lockout' ($left -gt 1500) "remaining=$left"
    $s = Wait-State 'triggered' 15
    Check 'a fast restart leaves the packs applied' ($s.state -eq 'triggered') "state=$($s.state)"
    Check 'a fast restart never reverts' (@(Get-ChangeTimes 'revert').Count -eq $reverts) "reverts=$reverts now=$(@(Get-ChangeTimes 'revert').Count)"
    Check 'a fast restart does not apply again' (@(Get-ChangeTimes 'apply').Count -eq $applies) "applies=$applies now=$(@(Get-ChangeTimes 'apply').Count)"

    # ---- 9b. a fast restart after the lockout ended never reverts ----
    # The close lands between two lockouts (2026-10-21): the close starts a window and holds
    # the revert, the restart inside that window cancels it, and the packs stay applied the
    # whole time.
    $applies = @(Get-ChangeTimes 'apply').Count
    $reverts = @(Get-ChangeTimes 'revert').Count
    $p4 | Stop-Process -Force
    Start-Sleep -Milliseconds 700
    Check 'a close after the lockout ended holds the revert' (@(Get-ChangeTimes 'revert').Count -eq $reverts) "reverts=$reverts now=$(@(Get-ChangeTimes 'revert').Count)"
    $p4 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    # The decision of 2026-10-23 ("the game wins"): the start cancels the revert the window was
    # holding, the machine stays in Turbo (the packs applied), and the window keeps running for
    # the rest of its time - the guard is still up for whatever the packs try next.
    $seen = $false
    for ($i = 0; $i -lt 40 -and -not $seen; $i++) {
        Start-Sleep -Milliseconds 100
        $s = Status
        $seen = ($s -and $s.trigger_source -eq 'timeout.exe')
    }
    Check 'the restart inside the window is seen' $seen "source=$(($s).trigger_source)"
    Check 'the restart leaves the window of the held revert running' (($s.state -eq 'cooldown') -and ($s.cooldown_remaining_ms -gt 0)) "state=$($s.state) remaining=$($s.cooldown_remaining_ms)"
    Check 'the restart cancels the change behind the window' ($null -eq $s.cooldown_action) "action=$($s.cooldown_action)"
    $s = Wait-State 'triggered' 20
    Check 'a fast restart after the lockout ended leaves the packs applied' ($s.state -eq 'triggered') "state=$($s.state)"
    Check 'a fast restart after the lockout ended never reverts' (@(Get-ChangeTimes 'revert').Count -eq $reverts) "reverts=$reverts now=$(@(Get-ChangeTimes 'revert').Count)"
    Check 'a fast restart after the lockout ended does not apply again' (@(Get-ChangeTimes 'apply').Count -eq $applies) "applies=$applies now=$(@(Get-ChangeTimes 'apply').Count)"

    # ---- 9c. a start while the lockout runs waits for it ----
    # The lockout holds a change in both directions (2026-10-22): an app that starts while the
    # daemon is still inside the window of the last change does *not* get the packs at once -
    # the apply waits for the window and lands when it is over, if the app is still running by
    # then. The revert below leaves the daemon in a window, so the start lands inside it.
    $applies = @(Get-ChangeTimes 'apply').Count
    $reverts = @(Get-ChangeTimes 'revert').Count
    $p4 | Stop-Process -Force
    $times = @(Wait-Change 'revert' $reverts 15)
    Check 'the close lets the revert land' ($times.Count -ge $reverts + 1) "reverts=$reverts now=$($times.Count)"
    $revertAt = $times[$times.Count - 1]
    $s = Wait-State 'cooldown' 8
    Check 'the revert starts a lockout of its own' ($s.state -eq 'cooldown') "state=$($s.state)"

    # the app starts again while that window still runs: the packs must not come back yet
    $left = (Status).cooldown_remaining_ms
    Check 'the start goes out inside the lockout' ($left -gt 2000) "remaining=$left"
    $startAt = Get-Date
    $p4 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    $times = @(Wait-Change 'apply' $applies 20)
    Check 'the held apply lands when the lockout expires' ($times.Count -ge $applies + 1) "applies=$applies now=$($times.Count)"
    $applyAt = $times[$times.Count - 1]
    Check 'the apply is not delayed past the lockout' ((Get-Gap $startAt $applyAt) -ge 2 -and (Get-Gap $startAt $applyAt) -le 12) "start=$startAt apply=$applyAt"
    Check 'the apply waited for the lockout' ((Get-Gap $revertAt $applyAt) -ge 3 -and (Get-Gap $revertAt $applyAt) -le 12) "revert=$revertAt apply=$applyAt"

    # the apply starts a lockout of its own, and the daemon settles again
    $s = Wait-State 'triggered' 20
    Check 'the daemon settles back to triggered' ($s.state -eq 'triggered') "state=$($s.state)"

    # ---- 10. a second tracked app changes nothing and starts no lockout ----
    # The packs are already applied, so the intent does not change: no apply and no lockout.
    Send-Pipe '{"type":"add_tracked_app","name":"Ping","exe":"C:\\Windows\\System32\\ping.exe"}' | Out-Null
    $applies = @(Get-ChangeTimes 'apply').Count
    $p5 = Start-Tracked 'C:\Windows\System32\ping.exe' '-n 20 127.0.0.1'
    Start-Sleep -Seconds 2
    $s = Status
    Check 'a second tracked app starts no lockout' (($s.state -eq 'triggered') -and ($null -eq $s.cooldown_remaining_ms)) "state=$($s.state) remaining=$($s.cooldown_remaining_ms)"
    Check 'a second tracked app does not apply again' (@(Get-ChangeTimes 'apply').Count -eq $applies) "applies=$applies now=$(@(Get-ChangeTimes 'apply').Count)"
    $p5 | Stop-Process -Force -ErrorAction SilentlyContinue
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\ping.exe"}' | Out-Null

    # ---- 11. pause stops automatic switching ----
    Send-Pipe '{"type":"set_paused","paused":true}' | Out-Null
    $p4 | Stop-Process -Force
    Start-Sleep -Seconds 1
    $s = Status
    Check 'paused: closing does not start a lockout' ($s.state -eq 'triggered') "state=$($s.state)"
    $p6 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    Start-Sleep -Seconds 1
    $s = Status
    Check 'paused: still applied while it runs' ($s.state -eq 'triggered') "state=$($s.state)"

    # ---- 12. removing the last tracked app reverts at once ----
    # A manual action bypasses the lockout: the packs go back right away and no lockout is
    # left behind.
    Send-Pipe '{"type":"set_paused","paused":false}' | Out-Null
    Start-Sleep -Milliseconds 300
    $reverts = @(Get-ChangeTimes 'revert').Count
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    $times = @(Wait-Change 'revert' $reverts 15)
    Check 'removing the last tracked app reverts at once' ($times.Count -ge $reverts + 1) "reverts=$reverts now=$($times.Count)"
    $s = Status
    Check 'the manual revert leaves no lockout behind' (($s.state -eq 'idle') -and ($null -eq $s.cooldown_remaining_ms)) "state=$($s.state) remaining=$($s.cooldown_remaining_ms)"
    $p6 | Stop-Process -Force -ErrorAction SilentlyContinue

    # ---- 12b. a change of the cooldown re-times the window that is running ----
    # The gap that let the defect through: the harness set the cooldown only *before* a
    # scenario, so a window that was already running was never re-timed, and the change the
    # window was holding landed at the old expiry. The window keeps its start and takes the
    # new length, so the change lands at start + cooldown.
    Send-Pipe '{"type":"add_tracked_app","name":"Timeout","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    Send-Pipe '{"type":"set_cooldown","seconds":5}' | Out-Null
    $startAt = Get-Date
    $p7 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    $s = Wait-State 'cooldown' 8
    Check 'the start opens a window' ($s.state -eq 'cooldown') "state=$($s.state)"
    $left = $s.cooldown_remaining_ms
    Check 'the window runs with the old cooldown' ($left -gt 2000 -and $left -le 5000) "remaining=$left"
    # the close is held by the window that is running
    $reverts = @(Get-ChangeTimes 'revert').Count
    $p7 | Stop-Process -Force
    $held = $null
    for ($i = 0; $i -lt 40 -and $null -eq $held; $i++) {
        Start-Sleep -Milliseconds 250
        $t = Status
        if ($t -and $t.cooldown_action -eq 'revert') { $held = $t }
    }
    Check 'the close is held by the running window' ($null -ne $held) "action=$(($held).cooldown_action)"
    # the user lengthens the cooldown while the window is running
    Send-Pipe '{"type":"set_cooldown","seconds":20}' | Out-Null
    Start-Sleep -Milliseconds 300
    $after = (Status).cooldown_remaining_ms
    Check 'the window in flight takes the new length' ($after -gt 14000 -and $after -le 20000) "before=$left now=$after"
    # and the change the window holds lands at the new expiry, not at the old one
    $times = @(Wait-Change 'revert' $reverts 40)
    Check 'the held revert lands on the re-timed window' ($times.Count -ge $reverts + 1) "reverts=$reverts now=$($times.Count)"
    $revertAt = $times[$times.Count - 1]
    Check 'the change lands at the new expiry' ((Get-Gap $startAt $revertAt) -ge 12 -and (Get-Gap $startAt $revertAt) -le 32) "start=$startAt revert=$revertAt"
    # the rest of the run uses the short cooldown again
    Send-Pipe '{"type":"set_cooldown","seconds":5}' | Out-Null
    $s = Wait-State 'idle' 20
    Check 'the daemon goes idle after the re-timed window' ($s.state -eq 'idle') "state=$($s.state)"
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null

    # ---- 12c. a change of the cooldown does not resurrect a window that has ended ----
    # The daemon drains a pipe request *before* its 1 Hz housekeeping runs `tick`, so a window
    # that has ended is still in the engine when a new length arrives. Re-timing it made a dead
    # window alive again: the dashboard showed a fresh countdown and the next game launch was
    # held for a window that never started (23 Oct review). This check waits for the report to
    # say the window is over - the engine still holds it - and then changes the cooldown in the
    # gap before the tick.
    Send-Pipe '{"type":"add_tracked_app","name":"Timeout","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    Send-Pipe '{"type":"set_cooldown","seconds":5}' | Out-Null
    $p9 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    $s = Wait-State 'cooldown' 8
    Check 'the start opens a window' ($s.state -eq 'cooldown') "state=$($s.state)"
    Start-Sleep -Milliseconds 500
    $applies = @(Get-ChangeTimes 'apply').Count
    $ended = $false
    for ($i = 0; $i -lt 400; $i++) {
        $s = Status
        if ($s -and $null -eq $s.cooldown_remaining_ms) { $ended = $true; break }
        Start-Sleep -Milliseconds 20
    }
    Check 'the window ends' $ended "remaining=$(($s).cooldown_remaining_ms)"
    Send-Pipe '{"type":"set_cooldown","seconds":60}' | Out-Null
    $s = Status
    Check 'the new length does not bring the ended window back' ($null -eq $s.cooldown_remaining_ms) "remaining=$($s.cooldown_remaining_ms)"
    Check 'the change leaves the machine settled' ($s.state -eq 'triggered') "state=$($s.state)"
    Check 'the change does not touch the packs' (@(Get-ChangeTimes 'apply').Count -eq $applies) "applies=$applies now=$(@(Get-ChangeTimes 'apply').Count)"
    Send-Pipe '{"type":"set_cooldown","seconds":5}' | Out-Null
    $p9 | Stop-Process -Force
    $s = Wait-State 'idle' 25
    Check 'the daemon goes idle after the window that ended' ($s.state -eq 'idle') "state=$($s.state)"
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null

    # ---- 13. config lives in the throw-away folder, not in %APPDATA% ----
    $tempConfig = Join-Path $configDir 'config.json'
    Check 'the test instance used its own config folder' (Test-Path $tempConfig) "expected $tempConfig"

    # ---- 14. settings persist to that config file ----
    # The dashboard always opens in the corner: there is no position to keep, and the request
    # that used to carry it is gone. The cooldown is set, so the config on disk is written
    # again and the check below sees what the daemon would persist.
    Send-Pipe '{"type":"set_cooldown","seconds":25}' | Out-Null
    Start-Sleep -Milliseconds 400
    $saved = Get-Content $tempConfig -Raw | ConvertFrom-Json
    Check 'cooldown persisted' ($saved.cooldown_seconds -eq 25) "got $($saved.cooldown_seconds)"
    Check 'the retired dashboard position is gone from the config' ($null -eq $saved.ui_position) "got $($saved.ui_position)"

    # ---- 15. install / remove a pack, strictly inside the build folder ----
    # packs_root = the first search dir that has a "packs" folder. Creating it beside the test exe
    # guarantees install/remove can never touch the repository (that mistake deleted a repo pack
    # once; the check below guards it).
    $script:buildPacksExisted = Test-Path -LiteralPath $buildPacks
    New-Item -ItemType Directory -Force -Path $buildPacks | Out-Null
    $packSource = (Resolve-Path (Join-Path $PSScriptRoot '..\packs\rainmeter\pack.json')).Path
    $installedDir = Join-Path $buildPacks 'rainmeter'
    # remove_pack deletes the whole pack folder, so a pack that was already in the build folder
    # before this run would be gone for good. Park it and put it back in the finally block.
    if (Test-Path -LiteralPath $installedDir) {
        $script:parkedPack = Join-Path $configDir 'parked-rainmeter'
        Move-Item -LiteralPath $installedDir -Destination $script:parkedPack -Force
    }
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($packSource.Replace('\','\\'))`"}" 'pack_op'
    Check 'install reports success' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Check 'install copied the pack into the build folder' (Test-Path (Join-Path $installedDir 'pack.json')) "expected $installedDir"
    Check 'install did not touch the repo' (Test-Path $packSource) "missing: $packSource"
    $op = Send-And-Wait '{"type":"remove_pack","id":"rainmeter"}' 'pack_op'
    Check 'remove reports success' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Check 'remove deleted the build copy' (-not (Test-Path $installedDir)) "still there: $installedDir"
    Check 'the repo copy still exists after remove' (Test-Path $packSource) "missing: $packSource"

    # ---- 16. pause is remembered across a restart (and the engine starts frozen) ----
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
    $p5 = Start-Tracked 'C:\Windows\System32\timeout.exe' '20'
    Start-Sleep -Seconds 2
    $s = Status
    Check 'a paused restart does not apply on start' ($s.state -eq 'idle') "state=$($s.state)"
    $p5 | Stop-Process -Force
    Send-Pipe '{"type":"set_paused","paused":false}' | Out-Null
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null

    # ---- 17. a duplicate start leaves the first instance alone ----
    $second = Start-Process $Exe -ArgumentList '--autostart' -PassThru -WindowStyle Hidden
    $exited = $second.WaitForExit(5000)
    Check 'a duplicate start exits by itself' $exited "still running after 5 s"
    Check 'the first instance is still up' (-not $daemon.HasExited)
    $realConfigAfter = Get-FileSha $realConfig
    Check 'the real config was not touched at all' ($realConfigAfter -eq $script:realConfigBefore) "before=$($script:realConfigBefore) after=$realConfigAfter"

    # ---- 18. rapid open/close churn must not leave a phantom state ----
    Send-Pipe '{"type":"set_paused","paused":false}' | Out-Null
    Send-Pipe '{"type":"set_cooldown","seconds":5}' | Out-Null
    Send-Pipe '{"type":"add_tracked_app","name":"Timeout","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    for ($i = 0; $i -lt 5; $i++) {
        $q = Start-Tracked 'C:\Windows\System32\timeout.exe' '20'
        Start-Sleep -Milliseconds 700
        $q | Stop-Process -Force
        Start-Sleep -Milliseconds 400
    }
    # after the last close the intent is gone: the packs revert as soon as a change is allowed
    # and the daemon ends up idle, once
    $s = Wait-State 'idle' 25
    Check 'churn settles back to idle' ($s.state -eq 'idle') "state=$($s.state)"
    Check 'churn leaves no lockout behind' ($null -eq $s.cooldown_remaining_ms) "remaining=$($s.cooldown_remaining_ms)"
    # ---- 19. a spec-2 pack: the daemon compiles and runs its script ----
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
    $p7 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    # the apply starts a lockout, so the state is "cooldown" first and "triggered" after it
    $s = Wait-State 'triggered' 20
    Start-Sleep -Milliseconds 800
    $logText = Get-Content (Join-Path $configDir 'logs\daemon.log') -Raw
    Check 'the apply script ran' ($logText -match 'script apply ran for timeout.exe') "no script log line"
    Check 'the apply script finished' ($logText -match 'scripttest: script apply finished')
    $p7 | Stop-Process -Force
    $s = Wait-State 'idle' 25
    Start-Sleep -Milliseconds 500
    $logText = Get-Content (Join-Path $configDir 'logs\daemon.log') -Raw
    Check 'the revert script ran' ($logText -match 'script revert ran')
    Send-Pipe '{"type":"set_pack_enabled","id":"scripttest","enabled":false}' | Out-Null
    $op = Send-And-Wait '{"type":"remove_pack","id":"scripttest"}' 'pack_op'
    Check 'the scripted pack removes cleanly' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    Remove-Item -Recurse -Force $srcDir -ErrorAction SilentlyContinue

    # ---- 19b. the host API is scoped: a pack may only call what it was granted ----
    # A pack declares the capabilities it wants. The daemon grants only those, and a call
    # to a function of a capability that was not granted fails as an *unknown function*:
    # the denial is structural, not a check a call site could forget. The denial names the
    # missing capability and what the pack asked for, and it lands in the pack's log.
    #
    # The denied call sits inside a function the script defines, because Rhai wraps an error
    # raised there (ErrorInFunctionCall): matching only the top-level error threw the denial
    # away and the log got Rhai's raw "Function not found" instead (23 Oct review).
    $capDir = Join-Path $env:TEMP "armoury-cappack-$ns"
    New-Item -ItemType Directory -Force -Path $capDir | Out-Null
    $capPack = @'
{
  "spec": 2,
  "id": "captest",
  "name": "Capability test",
  "version": "1.0.0",
  "order": 6,
  "targets": { "process": ["timeout.exe"] },
  "capabilities": ["files"],
  "script": {
    "apply":  "fn send_bang(host) { host.run_cli(\"C:\\\\Windows\\\\System32\\\\cmd.exe\", [\"/c\", \"echo hi\"]); } ctx.read_file(\"C:/armoury-capability-probe-does-not-exist.txt\"); send_bang(ctx);",
    "revert": "ctx.read_file(\"C:/armoury-capability-probe-does-not-exist.txt\");"
  },
  "apply": { "steps": [] },
  "revert": { "guard": { "targetRunning": false, "capturedRunning": true }, "steps": [] }
}
'@
    [IO.File]::WriteAllText((Join-Path $capDir 'pack.json'), $capPack, (New-Object System.Text.UTF8Encoding($false)))
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($capDir.Replace('\','\\'))`"}" 'pack_op'
    Check 'a pack that declares its capabilities installs' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Send-Pipe '{"type":"set_pack_enabled","id":"captest","enabled":true}' | Out-Null
    Send-Pipe '{"type":"set_cooldown","seconds":5}' | Out-Null
    Send-Pipe '{"type":"add_tracked_app","name":"Timeout","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    $p8 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    $s = Wait-State 'triggered' 25
    Start-Sleep -Milliseconds 800
    $logText = Get-Content (Join-Path $configDir 'logs\daemon.log') -Raw
    Check 'the granted call still ran' ($logText -match 'read_file C:/armoury-capability-probe-does-not-exist.txt') "the granted function of the pack did not run"
    Check 'the ungranted call is in the pack log' ($logText -match 'captest: script apply failed') "no failure line for the pack"
    Check 'the denial names the missing capability' ($logText -match "run_cli REFUSED: it needs the 'exec' capability") "no capability in the denial"
    Check 'the denial names what the pack asked for' ($logText -match 'the pack asked for: files') "no grant set in the denial"
    Check "the denial survives Rhai's wrapping" (-not ($logText -match 'Function not found')) "the raw Rhai message reached the log"
    $p8 | Stop-Process -Force
    Send-Pipe '{"type":"set_pack_enabled","id":"captest","enabled":false}' | Out-Null
    $op = Send-And-Wait '{"type":"remove_pack","id":"captest"}' 'pack_op'
    Check 'the pack with capabilities removes cleanly' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    Remove-Item -Recurse -Force $capDir -ErrorAction SilentlyContinue

    # the gate covers the declarative steps too: the script was gated and the steps were not,
    # so a pack that declared ["files"] could still run a program through a step (23 Oct review)
    $stepDir = Join-Path $env:TEMP "armoury-steppack-$ns"
    New-Item -ItemType Directory -Force -Path $stepDir | Out-Null
    $stepPack = @'
{
  "spec": 1,
  "id": "steptest",
  "name": "Step test",
  "version": "1.0.0",
  "order": 8,
  "targets": { "process": ["timeout.exe"] },
  "capabilities": ["files"],
  "apply": { "steps": [ { "mechanism": "cli", "exe": "C:\\Windows\\System32\\cmd.exe", "args": ["/c", "echo hi"] } ] },
  "revert": { "steps": [ { "mechanism": "cli", "exe": "C:\\Windows\\System32\\cmd.exe", "args": ["/c", "echo hi"] } ] }
}
'@
    [IO.File]::WriteAllText((Join-Path $stepDir 'pack.json'), $stepPack, (New-Object System.Text.UTF8Encoding($false)))
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($stepDir.Replace('\','\\'))`"}" 'pack_op'
    Check 'a step of a capability the pack did not ask for is refused' ($op -and -not $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Check 'the step refusal names the mechanism and the capability' ($op -and $op.message -match "'cli' needs the 'exec' capability") "message=$($op.message)"
    Check 'the step refusal names what the pack asked for' ($op -and $op.message -match 'the pack asked for: files') "message=$($op.message)"
    Check 'the refused step pack landed nowhere' (-not (Test-Path (Join-Path $buildPacks 'steptest'))) "the pack was installed anyway"
    Remove-Item -Recurse -Force $stepDir -ErrorAction SilentlyContinue

    # ---- 19c. the pack schema: `kind` and `triggers` ----
    # A pack may say what it is (`kind`: "pack" or "mode") and what starts it (`triggers`).
    # Both fields are **additive**: a pack that does not carry them loads and behaves
    # exactly as it did before, and the three shipped packs do not carry them. The mode's
    # own trigger machinery is a later phase - today a mode joins the ordinary trigger
    # path (on its own worker since 19d); what this checks is that the schema knows the
    # two fields, that a pack may carry them, and that a malformed one is refused by name.
    $kindDir = Join-Path $env:TEMP "armoury-kindpack-$ns"
    New-Item -ItemType Directory -Force -Path $kindDir | Out-Null
    $kindPack = @'
{
  "spec": 2,
  "id": "kindtest",
  "name": "Kind test",
  "version": "1.0.0",
  "order": 7,
  "kind": "mode",
  "triggers": [ { "kind": "hotkey", "key": "end" } ],
  "targets": { "process": ["timeout.exe"] },
  "script": { "apply": "ctx.log(\"kind apply ran\");", "revert": "ctx.log(\"kind revert ran\");" },
  "apply": { "steps": [] },
  "revert": { "guard": { "targetRunning": false, "capturedRunning": true }, "steps": [] }
}
'@
    [IO.File]::WriteAllText((Join-Path $kindDir 'pack.json'), $kindPack, (New-Object System.Text.UTF8Encoding($false)))
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($kindDir.Replace('\','\\'))`"}" 'pack_op'
    Check 'a pack with a kind and triggers installs' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    $s = Status
    Check 'the daemon loaded the pack' (@($s.packs | Where-Object { $_.id -eq 'kindtest' }).Count -eq 1) "packs=$(@($s.packs).Count)"
    Start-Sleep -Milliseconds 300
    $logText = Get-Content (Join-Path $configDir 'logs\daemon.log') -Raw
    Check 'the daemon read the kind of the pack' ($logText -match 'Kind test v.*kind mode') "the log does not name the kind"
    $op = Send-And-Wait '{"type":"remove_pack","id":"kindtest"}' 'pack_op'
    Check 'the pack with a kind removes cleanly' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Remove-Item -Recurse -Force $kindDir -ErrorAction SilentlyContinue

    # a malformed kind is a named error, like the other schema errors
    $badDir = Join-Path $env:TEMP "armoury-badkind-$ns"
    New-Item -ItemType Directory -Force -Path $badDir | Out-Null
    $badPack = $kindPack.Replace('"kind": "mode"', '"kind": "widget"')
    [IO.File]::WriteAllText((Join-Path $badDir 'pack.json'), $badPack, (New-Object System.Text.UTF8Encoding($false)))
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($badDir.Replace('\','\\'))`"}" 'pack_op'
    Check 'a malformed kind is refused' ($op -and -not $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Check 'the refusal names the kind' ($op -and $op.message -match 'kind') "message=$($op.message)"
    Check 'the refusal names the bad value' ($op -and $op.message -match 'widget') "message=$($op.message)"
    Check 'the refusal says what the kind may be' ($op -and $op.message -match "'pack'") "message=$($op.message)"
    Check 'the refused pack landed nowhere' (-not (Test-Path (Join-Path $buildPacks 'kindtest'))) "the pack was installed anyway"
    Remove-Item -Recurse -Force $badDir -ErrorAction SilentlyContinue

    # a mode needs at least one trigger
    $modeDir = Join-Path $env:TEMP "armoury-modepack-$ns"
    New-Item -ItemType Directory -Force -Path $modeDir | Out-Null
    $modePack = $kindPack.Replace('"triggers": [ { "kind": "hotkey", "key": "end" } ],', '"triggers": [],')
    [IO.File]::WriteAllText((Join-Path $modeDir 'pack.json'), $modePack, (New-Object System.Text.UTF8Encoding($false)))
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($modeDir.Replace('\','\\'))`"}" 'pack_op'
    Check 'a mode without a trigger is refused' ($op -and -not $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Check 'the refusal names the trigger' ($op -and $op.message -match 'trigger') "message=$($op.message)"
    Remove-Item -Recurse -Force $modeDir -ErrorAction SilentlyContinue

    # ---- 19d. a slow mode does not delay a plain pack: the mode has its own worker ----
    # Modes run on their own thread and channel (A4). Two modes sleep five seconds - one
    # inside its apply, one inside its revert - and the plain pack between them runs at
    # once. A tracked app start applies every enabled pack through whole-state jobs, so both
    # halves go out together, each on its own worker, and the log's own timestamps show the
    # plain pack finishing while a mode is still sleeping. The same holds when the app
    # closes: the plain pack goes back at once, the mode that reverts first sleeps five
    # seconds. `order` is what makes both directions mean something - the apply runs
    # ascending and the revert descending, so the mode slow in its *apply* sorts below the
    # pack and the mode slow in its *revert* sorts above it. On the single serial worker
    # this replaced, one of the two would always be ahead of the pack and the pack's line
    # would follow five seconds later, which is exactly what the two timing checks below
    # would report. Both workers feed one outcome channel, and each outcome carries the ids
    # its job was scoped to, so the daemon merges the halves order-independently. The log
    # resolves one second, so every bound keeps a margin of over two seconds.
    $slowDir = Join-Path $env:TEMP "armoury-slowpack-$ns"
    $fastDir = Join-Path $env:TEMP "armoury-fastpack-$ns"
    $slowRvDir = Join-Path $env:TEMP "armoury-slowrevert-$ns"
    New-Item -ItemType Directory -Force -Path $slowDir | Out-Null
    New-Item -ItemType Directory -Force -Path $fastDir | Out-Null
    New-Item -ItemType Directory -Force -Path $slowRvDir | Out-Null
    # The guards are unconditional ("targetRunning": false): a block *without* a guard
    # skips when its target process is not running, and the revert would be skipped
    # entirely once the app is dead - the check would watch a sleep that never runs.
    $slowPack = @'
{
  "spec": 1,
  "id": "slowmode",
  "name": "Slow test",
  "version": "1.0.0",
  "order": 9,
  "kind": "mode",
  "triggers": [ { "kind": "tracked_app" } ],
  "targets": { "process": ["timeout.exe"] },
  "apply": { "guard": { "targetRunning": false }, "steps": [ { "mechanism": "cli", "exe": "C:\\Windows\\System32\\cmd.exe", "args": ["/c", "exit 0"], "sleep_ms": 5000 } ] },
  "revert": { "guard": { "targetRunning": false }, "steps": [ { "mechanism": "cli", "exe": "C:\\Windows\\System32\\cmd.exe", "args": ["/c", "exit 0"] } ] }
}
'@
    $fastPack = @'
{
  "spec": 1,
  "id": "fastpack",
  "name": "Fast test",
  "version": "1.0.0",
  "order": 10,
  "targets": { "process": ["timeout.exe"] },
  "apply": { "guard": { "targetRunning": false }, "steps": [ { "mechanism": "cli", "exe": "C:\\Windows\\System32\\cmd.exe", "args": ["/c", "exit 0"] } ] },
  "revert": { "guard": { "targetRunning": false }, "steps": [ { "mechanism": "cli", "exe": "C:\\Windows\\System32\\cmd.exe", "args": ["/c", "exit 0"] } ] }
}
'@
    $slowRvPack = @'
{
  "spec": 1,
  "id": "slowrevert",
  "name": "Slow revert test",
  "version": "1.0.0",
  "order": 11,
  "kind": "mode",
  "triggers": [ { "kind": "tracked_app" } ],
  "targets": { "process": ["timeout.exe"] },
  "apply": { "guard": { "targetRunning": false }, "steps": [ { "mechanism": "cli", "exe": "C:\\Windows\\System32\\cmd.exe", "args": ["/c", "exit 0"] } ] },
  "revert": { "guard": { "targetRunning": false }, "steps": [ { "mechanism": "cli", "exe": "C:\\Windows\\System32\\cmd.exe", "args": ["/c", "exit 0"], "sleep_ms": 5000 } ] }
}
'@
    [IO.File]::WriteAllText((Join-Path $slowDir 'pack.json'), $slowPack, (New-Object System.Text.UTF8Encoding($false)))
    [IO.File]::WriteAllText((Join-Path $fastDir 'pack.json'), $fastPack, (New-Object System.Text.UTF8Encoding($false)))
    [IO.File]::WriteAllText((Join-Path $slowRvDir 'pack.json'), $slowRvPack, (New-Object System.Text.UTF8Encoding($false)))
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($slowDir.Replace('\','\\'))`"}" 'pack_op'
    Check 'the slow mode installs' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($fastDir.Replace('\','\\'))`"}" 'pack_op'
    Check 'the plain pack installs' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Start-Sleep -Milliseconds 500
    $logText = Get-Content $logPath -Raw
    Check 'the mode worker took the mode' ($logText -match 'mode worker started with 1 pack\(s\)') "no mode worker line in the log"
    # the second mode goes in after that check, so the "1 pack(s)" line above stays the mark
    # of a reload that saw one mode; this one turns it into two
    $op = Send-And-Wait "{`"type`":`"install_pack`",`"source`":`"$($slowRvDir.Replace('\','\\'))`"}" 'pack_op'
    Check 'the slow revert mode installs' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Send-Pipe '{"type":"set_pack_enabled","id":"slowmode","enabled":true}' | Out-Null
    Send-Pipe '{"type":"set_pack_enabled","id":"fastpack","enabled":true}' | Out-Null
    Send-Pipe '{"type":"set_pack_enabled","id":"slowrevert","enabled":true}' | Out-Null
    Send-Pipe '{"type":"set_cooldown","seconds":5}' | Out-Null
    Send-Pipe '{"type":"add_tracked_app","name":"Timeout","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    $p9 = Start-Tracked 'C:\Windows\System32\timeout.exe' '40'
    # the apply starts a lockout, so the state is "cooldown" first and "triggered" after it,
    # and the slow mode's five-second sleep runs on its own thread the whole time
    $s = Wait-State 'triggered' 25
    Check 'the tracked start applies all three' ($s.state -eq 'triggered') "state=$($s.state)"
    Start-Sleep -Seconds 7   # the mode's sleep plus the log's one-second resolution
    $send = @(Get-ChangeTimes 'apply')[-1]
    $fastAt = @(Get-PackLineTimes 'Fast test' 'applied')[-1]
    $slowAt = @(Get-PackLineTimes 'Slow test' 'applied')[-1]
    $fastGap = Get-Gap $send $fastAt
    $slowGap = Get-Gap $send $slowAt
    Check 'the plain pack applied without waiting' ($fastGap -le 3) "send=$send at=$fastAt gap=${fastGap}s"
    Check 'the mode applied on its own time' ($slowGap -ge 3.5 -and $slowGap -le 10) "send=$send at=$slowAt gap=${slowGap}s"
    $p9 | Stop-Process -Force
    $s = Wait-State 'idle' 25
    Check 'the close reverts and settles idle' ($s.state -eq 'idle') "state=$($s.state)"
    Start-Sleep -Seconds 8   # the slow revert's sleep plus the log's one-second resolution
    $rvSend = @(Get-ChangeTimes 'revert')[-1]
    $fastRv = @(Get-PackLineTimes 'Fast test' 'reverted')[-1]
    $slowRv = @(Get-PackLineTimes 'Slow revert test' 'reverted')[-1]
    $fastGap = Get-Gap $rvSend $fastRv
    $slowGap = Get-Gap $rvSend $slowRv
    Check 'the plain pack reverted without waiting' ($fastGap -le 3) "send=$rvSend at=$fastRv gap=${fastGap}s"
    Check 'the slow revert mode reverted on its own time' ($slowGap -ge 3.5 -and $slowGap -le 12) "send=$rvSend at=$slowRv gap=${slowGap}s"
    Send-Pipe '{"type":"set_pack_enabled","id":"slowmode","enabled":false}' | Out-Null
    Send-Pipe '{"type":"set_pack_enabled","id":"fastpack","enabled":false}' | Out-Null
    Send-Pipe '{"type":"set_pack_enabled","id":"slowrevert","enabled":false}' | Out-Null
    $op = Send-And-Wait '{"type":"remove_pack","id":"slowmode"}' 'pack_op'
    Check 'the slow mode removes cleanly' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    $op = Send-And-Wait '{"type":"remove_pack","id":"fastpack"}' 'pack_op'
    Check 'the plain pack removes cleanly' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    $op = Send-And-Wait '{"type":"remove_pack","id":"slowrevert"}' 'pack_op'
    Check 'the slow revert mode removes cleanly' ($op -and $op.ok) "reply=$($op | ConvertTo-Json -Compress)"
    Send-Pipe '{"type":"remove_tracked_app","exe":"C:\\Windows\\System32\\timeout.exe"}' | Out-Null
    Remove-Item -Recurse -Force $slowDir -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $fastDir -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $slowRvDir -ErrorAction SilentlyContinue

    # ---- 19e. the killswitch has a config key, rides the pipe and survives a restart ----
    # One key does double duty - the config switch and the "is the killswitch module
    # installed" flag: set_killswitch over the pipe flips it, the config file carries it,
    # a restart honours it, and the fresh default is on (an older config has no key at all).
    $s = Status
    Check 'a fresh killswitch flag defaults to on' ($s -and $s.killswitch_enabled) "killswitch_enabled=$($s.killswitch_enabled)"
    Send-Pipe '{"type":"set_killswitch","enabled":false}' | Out-Null
    $s = $null
    for ($i = 0; $i -lt 30; $i++) { Start-Sleep -Milliseconds 150; $s = Status; if ($s -and -not $s.killswitch_enabled) { break } }
    Check 'set_killswitch over the pipe turns the flag off' ($s -and -not $s.killswitch_enabled) "killswitch_enabled=$($s.killswitch_enabled)"
    $saved = Get-Content $tempConfig -Raw | ConvertFrom-Json
    Check 'the killswitch off state persisted to the config' ($saved.killswitch_enabled -eq $false) "got $($saved.killswitch_enabled)"
    $daemon | Stop-Process -Force
    Start-Sleep -Seconds 1
    $daemon = Start-Process $Exe -PassThru -WindowStyle Hidden
    $s = $null
    for ($i = 0; $i -lt 40 -and -not $s; $i++) { Start-Sleep -Milliseconds 250; $s = Status }
    Check 'the restarted instance answers' ($null -ne $s)
    Check 'the killswitch off state survives the restart' ($s -and -not $s.killswitch_enabled) "killswitch_enabled=$($s.killswitch_enabled)"
    Send-Pipe '{"type":"set_killswitch","enabled":true}' | Out-Null
    $s = $null
    for ($i = 0; $i -lt 30; $i++) { Start-Sleep -Milliseconds 150; $s = Status; if ($s -and $s.killswitch_enabled) { break } }
    Check 'set_killswitch over the pipe turns the flag back on' ($s -and $s.killswitch_enabled) "killswitch_enabled=$($s.killswitch_enabled)"
    $saved = Get-Content $tempConfig -Raw | ConvertFrom-Json
    Check 'the killswitch on state persisted to the config' ($saved.killswitch_enabled -eq $true) "got $($saved.killswitch_enabled)"
    Check 'the log names the disable' ((Test-Path -LiteralPath $logPath) -and [bool](Select-String -LiteralPath $logPath -Pattern 'killswitch.*disable' -Quiet)) "pattern killswitch.*disable against $logPath"

    # ---- 20. the command line: the modes that print and exit ----
    # The command line is parsed before anything else happens, so these runs never touch the
    # desktop. They run in a namespace of their own ($cliNs), see the top of this file.
    #
    # The executable asks Windows for administrator rights (the manifest it carries), so a
    # probe started from a shell that is not elevated raises a UAC prompt - and the harness
    # would sit there waiting for an answer. An elevated harness hands the probes its own
    # elevated token: no prompt, and the probes run exactly as they always did. A harness
    # that is not elevated skips them and says why - a skipped check is not a failed one.
    if (Test-Elevated) {
        $env:ARMOURY_TEST_NS = $cliNs

        # --version: the version, exit 0
        $probe = Invoke-CliProbe @('--version')
        $r = $probe.Run
        Check 'version exits 0' ($r.Exit -eq 0) $probe.Detail
        Check 'version prints the version' ($r.Text.Contains('ArmouryDaemon 1.2.0')) "output=$(Get-FirstLine $r.Text)"

        # --help: the usage, exit 0, with every flag the parser accepts
        $probe = Invoke-CliProbe @('--help')
        $r = $probe.Run
        Check 'help exits 0' ($r.Exit -eq 0) $probe.Detail
        $flags = @('--autostart', '--ui', '--console', '--catalog-list', '--catalog-install', '--catalog-url', '--module-list', '--version', '--help')
        $missing = @($flags | Where-Object { -not $r.Text.Contains($_) })
        Check 'help lists every flag the binary accepts' ($missing.Count -eq 0) "missing: $($missing -join ' ')"

        # an argument the binary does not know is an error, never a daemon
        $probe = Invoke-CliProbe @('--definitely-not-a-flag')
        $r = $probe.Run
        Check 'an unknown argument exits 2' ($r.Exit -eq 2) $probe.Detail
        Check 'an unknown argument names itself' ($r.Text.Contains('--definitely-not-a-flag')) "output=$(Get-FirstLine $r.Text)"
        Check 'an unknown argument prints the usage on stderr' (($r.Out.Trim() -eq '') -and $r.Err.Contains('usage:')) "stdout=$(Get-FirstLine $r.Out) stderr=$(Get-FirstLine $r.Err)"
        $detail = "before=$($probe.Before) after=$($probe.After)"
        if ($probe.Killed) { $detail += ' (a stray daemon was killed)' }
        Check 'an unknown argument leaves no daemon running' ($probe.After -le $probe.Before) $detail

        # a flag without its value is an error as well
        $probe = Invoke-CliProbe @('--catalog-install')
        $r = $probe.Run
        Check 'catalog-install without an id exits 2' ($r.Exit -eq 2) $probe.Detail
        Check 'catalog-install without an id says the id is missing' ($r.Text.Contains('--catalog-install needs a pack id')) "output=$(Get-FirstLine $r.Text)"
        Check 'catalog-install without an id prints the usage on stderr' (($r.Out.Trim() -eq '') -and $r.Err.Contains('usage:')) "stdout=$(Get-FirstLine $r.Out) stderr=$(Get-FirstLine $r.Err)"
        $detail = "before=$($probe.Before) after=$($probe.After)"
        if ($probe.Killed) { $detail += ' (a stray daemon was killed)' }
        Check 'catalog-install without an id leaves no daemon running' ($probe.After -le $probe.Before) $detail

        # -- 20b. the catalogue fetch itself --
        #
        # The catalogue is fetched over HTTPS with the **Windows certificate store** as the
        # root store, and the response is compressed. Two probes show both:
        #
        # * a plain HTTP server on the loopback interface that answers with a
        #   **gzip-compressed** manifest. The daemon has to list the pack, and the server has
        #   to have seen "Accept-Encoding: gzip" - the client asks for a compressed response
        #   and decodes it.
        # * the **real** catalogue over HTTPS. The certificate of a public host is in the
        #   Windows store, so a fetch that succeeds proves the platform verifier is wired up
        #   and works. A machine behind a corporate CA cannot be shown here: that needs a
        #   private root in the machine's certificate store. The root store itself is
        #   asserted by the unit test in catalog.rs (the client trusts the platform
        #   verifier).

        # (a) the gzip manifest - and its minisign signature: an unsigned manifest is
        # refused since B2, so the probe serves the manifest and, when the client asks for
        # it, the .minisig beside it. Both are frozen fixture files (tools/fixtures): been
        # signed once by a key whose private half died with the throwaway folder it lived
        # in, so anyone in the repository can serve these exact bytes.
        $fixtures = Join-Path $PSScriptRoot 'fixtures'
        $testTrust = Join-Path $cliDir 'test-keyring.json'
        New-Item -ItemType Directory -Force -Path $cliDir | Out-Null
        @'
{"schema":1,"keys":[{"comment":"the harness's frozen probe key (private half deleted the day the fixtures were signed)","key":"RWTUa5h/DwHow3IGIQ9hySXFqYn7fQTiXHT6t0m5gwKCTcsY1eCakbOz"},{"comment":"the B3 helper fixtures' probe key (private half deleted the day the fixtures were signed)","key":"RWQDWVat2lgAnsBkYFGLiecUYzI3PoFbek2aOwBC9dmn2fYA0g/4d4L7"}]}
'@ | Set-Content -Path $testTrust -Encoding ascii
        $manText = [IO.File]::ReadAllText((Join-Path $fixtures 'man-old-key.json'))
        $gzipBody = Get-GzipBytes $manText
        $sigBytes = [IO.File]::ReadAllBytes((Join-Path $fixtures 'man-old-key.json.minisig'))
        $server = Start-LoopbackListener
        $script:served = ''
        $env:ARMOURY_TEST_TRUST = $testTrust
        $probe = Invoke-CatalogProbe @('--catalog-list', '--catalog-url', "http://127.0.0.1:$($server.Port)/catalog/index.json") {
            for ($i = 0; $i -lt 2; $i++) {
                # the first accept is the task the helper started; the signature fetch
                # opens its own connection, so it needs a fresh one
                if ($i -eq 0) { $accept = $server.Accept } else { $accept = $server.Listener.AcceptTcpClientAsync() }
                if (-not $accept.Wait(20000)) { throw "the daemon did not connect (request $($i + 1))" }
                $client = $accept.GetAwaiter().GetResult()
                try {
                    $script:served += Read-Request $client
                    $stream = $client.GetStream()
                    if ($i -eq 0) {
                        $head = "HTTP/1.1 200 OK`r`nContent-Type: application/json`r`nContent-Encoding: gzip`r`nContent-Length: $($gzipBody.Length)`r`nConnection: close`r`n`r`n"
                        $body = $gzipBody
                    }
                    else {
                        $head = "HTTP/1.1 200 OK`r`nContent-Type: text/plain`r`nContent-Length: $($sigBytes.Length)`r`nConnection: close`r`n`r`n"
                        $body = $sigBytes
                    }
                    $headBytes = [System.Text.Encoding]::ASCII.GetBytes($head)
                    $stream.Write($headBytes, 0, $headBytes.Length)
                    $stream.Write($body, 0, $body.Length)
                    $stream.Flush()
                }
                finally { $client.Close() }
            }
        }
        $env:ARMOURY_TEST_TRUST = $null
        $server.Listener.Stop()
        $r = $probe
        Check 'the catalogue listing succeeds against a local server' ($r.Exited -and $r.Exit -eq 0) "exit=$($r.Exit)"
        Check 'the catalogue listing names the pack' ($r.Text.Contains('local-test')) "output=$(Get-FirstLine $r.Text)"
        Check 'the client asks for a compressed response' ($script:served -match '(?i)accept-encoding:\s*gzip') "request=$($script:served -replace "`r`n", ' ')"

        # (a2) a schema-2 manifest: the entries under `modules`, naming a kind - and signed.
        # The gzip probe above serves the old `packs` key, which keeps working for one
        # release; this probe proves the new key lists too, the shape the packs repo will
        # publish at the flip.
        $v2Body = [IO.File]::ReadAllBytes((Join-Path $fixtures 'man-schema2.json'))
        $v2Sig = [IO.File]::ReadAllBytes((Join-Path $fixtures 'man-schema2.json.minisig'))
        $server = Start-LoopbackListener
        $env:ARMOURY_TEST_TRUST = $testTrust
        $probe = Invoke-CatalogProbe @('--catalog-list', '--catalog-url', "http://127.0.0.1:$($server.Port)/catalog/index.json") {
            for ($i = 0; $i -lt 2; $i++) {
                if ($i -eq 0) { $accept = $server.Accept } else { $accept = $server.Listener.AcceptTcpClientAsync() }
                if (-not $accept.Wait(20000)) { throw "the daemon did not connect (request $($i + 1))" }
                $client = $accept.GetAwaiter().GetResult()
                try {
                    Read-Request $client | Out-Null
                    $stream = $client.GetStream()
                    if ($i -eq 0) {
                        $head = "HTTP/1.1 200 OK`r`nContent-Type: application/json`r`nContent-Length: $($v2Body.Length)`r`nConnection: close`r`n`r`n"
                        $body = $v2Body
                    }
                    else {
                        $head = "HTTP/1.1 200 OK`r`nContent-Type: text/plain`r`nContent-Length: $($v2Sig.Length)`r`nConnection: close`r`n`r`n"
                        $body = $v2Sig
                    }
                    $headBytes = [System.Text.Encoding]::ASCII.GetBytes($head)
                    $stream.Write($headBytes, 0, $headBytes.Length)
                    $stream.Write($body, 0, $body.Length)
                    $stream.Flush()
                }
                finally { $client.Close() }
            }
        }
        $env:ARMOURY_TEST_TRUST = $null
        $server.Listener.Stop()
        Check 'the schema-2 manifest lists through modules' ($probe.Exited -and $probe.Exit -eq 0) "exit=$($probe.Exit)"
        Check 'the schema-2 listing names the pack' ($probe.Text.Contains('mode-test')) "output=$(Get-FirstLine $probe.Text)"

        # (b) the real catalogue, over HTTPS, with the Windows certificate store
        # The catalogue is signed since B2: its manifest's signature fetch must succeed
        # before the listing happens. While the live index.json has no index.json.minisig
        # beside it (the prepared-but-unpushed packs change is at 539773c), these two
        # checks skip and say why; they re-arm the moment the flip lands.
        $run = Invoke-Cli @('--catalog-list') 40
        $failure = Get-FetchFailure $run.Text
        if (($run.Exit -ne 0) -and ($failure -eq 'network')) {
            # the machine cannot reach the catalogue right now. That is the network and not
            # the build, so the check is skipped rather than failed.
            "SKIP  the catalogue fetch against the real catalogue: this machine cannot reach"
            "      it right now ($($run.Err.Trim()))"
        }
        elseif (($run.Exit -ne 0) -and ($failure -eq 'signature')) {
            "SKIP  the catalogue fetch against the real catalogue: the live manifest is not"
            "      signed yet (tools/fixtures carry the probes; the packs flip at 539773c"
            "      publishes index.json.minisig with the next release). ($($run.Err.Trim()))"
        }
        else {
            # a certificate error here is a failure of the build: the platform verifier is
            # not doing its job, and the fetch of a public host has to work
            Check 'the catalogue fetch validates against the Windows certificate store' (($run.Exit -eq 0) -and ($run.Text -match 'catalog: https://')) "output=$(Get-FirstLine $run.Text)"
            Check 'the catalogue listing names the shipped packs' ($run.Text -match 'g-helper') "output=$(Get-FirstLine $run.Text)"
        }

        # (c) a pack from the real catalogue: the download, and the size and the SHA-256 of
        # the published bytes. This is the path a user takes when they install a pack, and
        # it is where a compressed response would show up: the bytes that are verified are
        # the plain ones.
        $run = Invoke-Cli @('--catalog-install', 'g-helper') 60
        $failure = Get-FetchFailure $run.Text
        if (($run.Exit -ne 0) -and ($failure -eq 'network')) {
            "SKIP  the catalogue install against the real catalogue: this machine cannot reach"
            "      it right now ($($run.Err.Trim()))"
        }
        elseif (($run.Exit -ne 0) -and ($failure -eq 'signature')) {
            "SKIP  the catalogue install against the real catalogue: the live manifest is not"
            "      signed yet (the packs flip at 539773c publishes index.json.minisig with the"
            "      next release). ($($run.Err.Trim()))"
        }
        else {
            Check 'a pack from the catalogue installs and verifies' ($run.Exit -eq 0) "output=$(Get-FirstLine $run.Text)"
        }

        # -- 20c. one client, not one per request --
        #
        # The manifest and the pack file are two requests to the same host, and they travel
        # over **one** connection. A client of its own for every request would open a
        # connection for each - and would rebuild the rustls config and the platform verifier
        # every time.
        #
        # The pack is the shipped G-Helper pack under a name of its own, so the probe installs
        # nothing that is really there: the folder it leaves behind is removed again below.
        # B2: the manifest and its signature are frozen fixtures (signed once by the probe
        # key), and the pack copy is frozen with them - a change to packs/g-helper/pack.json
        # does not move these bytes, so the signature keeps verifying.
        $fixtures = Join-Path $PSScriptRoot 'fixtures'
        $packBytes = [IO.File]::ReadAllBytes((Join-Path $fixtures 'pack-harness-test.json'))
        $manifestBytes = [IO.File]::ReadAllBytes((Join-Path $fixtures 'man-install.json'))
        $installSig = [IO.File]::ReadAllBytes((Join-Path $fixtures 'man-install.json.minisig'))
        $env:ARMOURY_TEST_TRUST = $testTrust
        $server = Start-LoopbackListener
        $script:requests = @()
        $script:connections = 0
        $probe = Invoke-CatalogProbe @('--catalog-install', 'harness-test', '--catalog-url', "http://127.0.0.1:$($server.Port)/catalog/index.json") {
            # the server answers until the daemon has all three files (manifest, its
            # signature, the pack), or until it runs out of time
            $deadline = (Get-Date).AddSeconds(20)
            while ((Get-Date) -lt $deadline -and $script:requests.Count -lt 3) {
                # the helper already started the first accept: a second one would compete with
                # it for the connection, and the daemon would wait for an answer that never
                # comes
                if ($script:connections -eq 0) { $accept = $server.Accept } else { $accept = $server.Listener.AcceptTcpClientAsync() }
                if (-not $accept.Wait(2000)) { break }   # no further connection
                $client = $accept.GetAwaiter().GetResult()
                $script:connections++
                $client.ReceiveTimeout = 5000
                try {
                    while ($true) {
                        try { $request = Read-Request $client } catch { break }
                        if (-not $request) { break }
                        $script:requests += $request
                        $route = ($request -split "`r`n")[0]
                        $body = if ($route -like '*index.json.minisig*') { $installSig }
                                elseif ($route -like '*pack.json*') { $packBytes }
                                else { $manifestBytes }
                        $head = "HTTP/1.1 200 OK`r`nContent-Type: application/json`r`nContent-Length: $($body.Length)`r`n`r`n"
                        $headBytes = [Text.Encoding]::ASCII.GetBytes($head)
                        $stream = $client.GetStream()
                        $stream.Write($headBytes, 0, $headBytes.Length)
                        $stream.Write($body, 0, $body.Length)
                        $stream.Flush()
                        if ($script:requests.Count -ge 3) { break }
                    }
                }
                finally { $client.Close() }
            }
        }
        $server.Listener.Stop()
        $env:ARMOURY_TEST_TRUST = $null
        # the pack the probe installed is not one of ours: it goes away again, from wherever
        # the daemon put it (the build folder if it is there, the repository's own folder if
        # the daemon resolved that one)
        Remove-Item -LiteralPath (Join-Path $buildPacks 'harness-test') -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath (Join-Path $repoRoot 'packs\harness-test') -Recurse -Force -ErrorAction SilentlyContinue
        Check 'the pack installs from the local catalogue' ($probe.Exited -and $probe.Exit -eq 0) "exit=$($probe.Exit) $(Get-FirstLine $probe.Text)"
        Check 'the manifest, its signature and the pack travel over one connection' (($script:connections -eq 1) -and ($script:requests.Count -eq 3)) "connections=$($script:connections) requests=$($script:requests.Count)"

        # -- 20d. a helper module installs the whole bundle --
        #
        # A helper entry downloads an archive instead of a pack file: manifest, minisign
        # signature, zip - three files over **one** connection (trap 55: each connection
        # waits on a fresh accept, the pre-started task answers once). The daemon checks the
        # archive's SHA-256 and size against the signed manifest, extracts into
        # modules/<id>/, and records the entry in the folder's module.json. A second probe
        # process lists what is installed - a fresh process reads only disk again, so what
        # it finds is what a restart would find.
        # The manifest and its signature are frozen fixtures signed by a probe key whose
        # private half died with the throwaway folder it was generated in
        # (tools/fixtures/helper-test.pub holds the verifying key).
        $helperZip = [IO.File]::ReadAllBytes((Join-Path $fixtures 'helper-harness-test.zip'))
        $helperMan = [IO.File]::ReadAllBytes((Join-Path $fixtures 'man-helper.json'))
        $helperSig = [IO.File]::ReadAllBytes((Join-Path $fixtures 'man-helper.json.minisig'))
        $env:ARMOURY_TEST_TRUST = $testTrust
        $server = Start-LoopbackListener
        $script:requests = @()
        $script:connections = 0
        $probe = Invoke-CatalogProbe @('--catalog-install', 'h-harness', '--catalog-url', "http://127.0.0.1:$($server.Port)/catalog/index.json") {
            # the server answers until the daemon has all three files (manifest, signature,
            # archive), or until it runs out of time
            $deadline = (Get-Date).AddSeconds(20)
            while ((Get-Date) -lt $deadline -and $script:requests.Count -lt 3) {
                # the first accept is the task the helper started; every connection but the
                # first needs a fresh one
                if ($script:connections -eq 0) { $accept = $server.Accept } else { $accept = $server.Listener.AcceptTcpClientAsync() }
                if (-not $accept.Wait(2000)) { break }
                $client = $accept.GetAwaiter().GetResult()
                $script:connections++
                $client.ReceiveTimeout = 5000
                try {
                    while ($true) {
                        try { $request = Read-Request $client } catch { break }
                        if (-not $request) { break }
                        $script:requests += $request
                        $route = ($request -split "`r`n")[0]
                        $body = if ($route -like '*index.json.minisig*') { $helperSig }
                                elseif ($route -like '*console.zip*') { $helperZip }
                                else { $helperMan }
                        $head = "HTTP/1.1 200 OK`r`nContent-Type: application/octet-stream`r`nContent-Length: $($body.Length)`r`n`r`n"
                        $headBytes = [Text.Encoding]::ASCII.GetBytes($head)
                        $stream = $client.GetStream()
                        $stream.Write($headBytes, 0, $headBytes.Length)
                        $stream.Write($body, 0, $body.Length)
                        $stream.Flush()
                        if ($script:requests.Count -ge 3) { break }
                    }
                }
                finally { $client.Close() }
            }
        }
        $server.Listener.Stop()
        $env:ARMOURY_TEST_TRUST = $null
        # what a fresh process reads back from disk: the install landed, its record is
        # readable - this is the helper found again after a restart
        $list = Invoke-Cli @('--module-list') 40
        $helperDir = $null
        foreach ($candidate in @((Join-Path $exeDir 'modules\h-harness\ArmouryConsole.exe'), (Join-Path $repoRoot 'modules\h-harness\ArmouryConsole.exe'))) {
            if (Test-Path -LiteralPath $candidate) { $helperDir = $candidate; break }
        }
        Check 'a helper installs from the local catalogue' ($probe.Exited -and $probe.Exit -eq 0) "exit=$($probe.Exit) $(Get-FirstLine $probe.Text)"
        Check 'the manifest, its signature and the helper archive travel over one connection' (($script:connections -eq 1) -and ($script:requests.Count -eq 3)) "connections=$($script:connections) requests=$($script:requests.Count)"
        Check 'the helper lands with its recorded entry' ($null -ne $helperDir) "none of the candidates holds modules\h-harness\ArmouryConsole.exe"
        Check 'the helper is listed by a fresh process (found after a restart)' ($list.Text.Contains('h-harness') -and $list.Text.Contains('helper')) "output=$(Get-FirstLine $list.Text)"

        # the helper the probe installed is not one of ours: it goes away again, from
        # wherever the daemon put it
        Remove-Item -LiteralPath (Join-Path $exeDir 'modules\h-harness') -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath (Join-Path $repoRoot 'modules\h-harness') -Recurse -Force -ErrorAction SilentlyContinue

        # -- 20e. a helper installs through the daemon's request (B5) --
        #
        # The Modules window downloads the helper archive itself and hands the daemon the
        # `InstallHelper` request, naming the manifest entry's version, executable path
        # and the SHA-256 the download was verified against. The daemon re-checks the
        # bytes before they land anywhere, extracts into modules/<id>/ under its locks,
        # and the temp file does not survive the answer; a fresh probe process lists what
        # a restart would find again.
        $requestZip = Join-Path $env:TEMP 'armoury-catalog-h-request.zip'
        Copy-Item -LiteralPath (Join-Path $fixtures 'helper-harness-test.zip') -Destination $requestZip -Force
        $requestHash = (Get-FileHash -LiteralPath $requestZip -Algorithm SHA256).Hash.ToLowerInvariant()
        $requestSize = (Get-Item -LiteralPath $requestZip).Length
        $requestJson = '{"type":"install_helper","id":"h-request","name":"Requested helper","version":"0.9.9","entry":"ArmouryConsole.exe","file":"' + ($requestZip -replace '\\', '\\') + '","sha256":"' + $requestHash + '","size":' + $requestSize + '}'
        $reply = Send-Pipe $requestJson
        Check 'the daemon answers the helper request' ($reply -match '"ok"\s*:\s*true') "reply=$(Get-FirstLine $reply)"
        $list = Invoke-Cli @('--module-list') 40
        Check 'the requested helper is listed by a fresh process' ($list.Text.Contains('h-request')) "output=$(Get-FirstLine $list.Text)"
        Check 'the requested helper archive left temp after the answer' (-not (Test-Path -LiteralPath $requestZip)) "still there: $requestZip"
        Remove-Item -LiteralPath (Join-Path $exeDir 'modules\h-request') -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath (Join-Path $repoRoot 'modules\h-request') -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $requestZip -Force -ErrorAction SilentlyContinue

        # -- 20f. show_console prefers an installed console helper (Phase C1/C2) --
        #
        # The console is its own exe now (ArmouryConsole.exe), and a helper module that
        # carries it is installed through the B5 request. show_console must prefer that
        # module's exe (`modules/<id>/ArmouryConsole.exe`) over the `--console` fallback on
        # the daemon binary. The fixture's exe is a placeholder that cannot run (bad PE
        # image), so the daemon names the choice in its log and the spawn fails - the band
        # never opens in a headless run. Removing the module brings the fallback back; that
        # probe really starts the console band, so the child is stopped by the pid the
        # daemon logged.
        $consoleZip = Join-Path $env:TEMP 'armoury-catalog-h-console.zip'
        Copy-Item -LiteralPath (Join-Path $fixtures 'helper-harness-test.zip') -Destination $consoleZip -Force
        $consoleHash = (Get-FileHash -LiteralPath $consoleZip -Algorithm SHA256).Hash.ToLowerInvariant()
        $consoleSize = (Get-Item -LiteralPath $consoleZip).Length
        $consoleJson = '{"type":"install_helper","id":"h-console","name":"Console helper","version":"1.2.0","entry":"ArmouryConsole.exe","file":"' + ($consoleZip -replace '\\', '\\') + '","sha256":"' + $consoleHash + '","size":' + $consoleSize + '}'
        $reply = Send-Pipe $consoleJson
        Check 'the console helper installs' ($reply -match '"ok"\s*:\s*true') "reply=$(Get-FirstLine $reply)"

        Send-Pipe '{"type":"show_console"}' | Out-Null
        $helperLine = Wait-DaemonLogLine 'console launcher: using module h-console v1\.2\.0'
        Check 'show_console uses the installed helper' ($null -ne $helperLine) "log=$(Get-FirstLine $helperLine)"
        $fallbackLines = @(((Read-Text $logPath) -split "`r?`n") | Where-Object { $_ -match 'no helper module' })
        Check 'the fallback stays quiet while a helper is installed' (@($fallbackLines).Count -eq 0) "count=$(@($fallbackLines).Count)"
        $failLine = Wait-DaemonLogLine 'failed to spawn the console launcher'
        Check 'the placeholder helper names itself at the spawn' ($null -ne $failLine) 'no spawn attempt went at the module exe'

        # the helper goes away again: the choice has to fall back to the daemon exe
        Remove-Item -LiteralPath (Join-Path $exeDir 'modules\h-console') -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath (Join-Path $repoRoot 'modules\h-console') -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $consoleZip -Force -ErrorAction SilentlyContinue
        Send-Pipe '{"type":"show_console"}' | Out-Null
        $fallbackLine = Wait-DaemonLogLine 'console launcher: no helper module'
        Check 'show_console falls back to the daemon exe' (($null -ne $fallbackLine) -and ($fallbackLine.Contains('--console'))) "log=$(Get-FirstLine $fallbackLine)"
        $childLine = Wait-DaemonLogLine 'spawned console launcher \(pid (\d+)\)'
        Check 'the fallback spawns a console launcher' ($null -ne $childLine) 'no pid in the log'
        if ($childLine -match 'pid (\d+)') {
            # the band is up for a moment; then it is stopped - the daemon keeps the
            # child as its launcher, and the probe is done with it
            Stop-Process -Id ([int]$Matches[1]) -Force -ErrorAction SilentlyContinue
        }

        $env:ARMOURY_TEST_NS = $ns
    }
    else {
        "SKIP  the command-line probes (--version, --help, an unknown argument, a flag without"
        "      its value) and the catalogue probes (a gzip manifest, the real catalogue, the local"
        "      one):"
        "      the executable asks Windows for administrator rights, and this harness is not"
        "      elevated - every probe would raise a UAC prompt and the harness would wait for"
        "      the answer. Run the harness from an elevated shell to run them."
    }

    # ---- 21. the root store the binary carries ----
    #
    # The build has exactly **one** root store: the Windows certificate store. The Mozilla
    # bundle is not in the binary, and the HTTP client refuses the bundled roots instead of
    # using them - so the catalogue, the cover art and the band's thumbnails can only be
    # validated against the Windows store. The unit tests in catalog.rs, art.rs and
    # console_ui.rs say that they use that client; these checks say that there is no other
    # one to use.
    #
    # These checks read the binary, so they need no elevation and they run either way.
    $binary = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($Exe))
    # the subject of one of Mozilla's bundled roots: it is in the binary as long as the
    # bundle is
    Check 'the binary carries no bundled root certificates' (-not $binary.Contains('DigiCert TLS ECC P384 Root G5')) 'the Mozilla root bundle is in the binary'
    # ureq's own refusal, compiled in when the bundled roots are not linked: the client
    # cannot fall back to them
    Check 'the client refuses the bundled roots instead of using them' ($binary.Contains('WebPki is disabled')) 'the refusal is not in the binary'
    # the Windows verifier, called by the platform verifier: the store is really used
    Check 'the binary links the Windows certificate verifier' ($binary.Contains('CertVerifyCertificateChainPolicy')) 'the Windows verifier is not linked'
}
finally {
    if ($daemon -and -not $daemon.HasExited) {
        $daemon | Stop-Process -Force -ErrorAction SilentlyContinue
    }
    # only the processes this harness started - never every timeout.exe on the machine
    foreach ($child in $script:children) {
        if (-not $child) { continue }
        try {
            if (-not $child.HasExited) { Stop-Process -Id $child.Id -Force -ErrorAction SilentlyContinue }
        } catch {
        }
    }
    # put a pack that was already in the build folder back where it was
    if ($script:parkedPack) {
        Remove-Item -LiteralPath (Join-Path $buildPacks 'rainmeter') -Recurse -Force -ErrorAction SilentlyContinue
        Move-Item -LiteralPath $script:parkedPack -Destination (Join-Path $buildPacks 'rainmeter') -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -Recurse -Force $configDir -ErrorAction SilentlyContinue
    # the config folder of the command-line namespace, if a probe ever started a daemon
    Remove-Item -Recurse -Force $cliConfigDir -ErrorAction SilentlyContinue
    # only remove the build-folder packs dir if this run created it
    if (-not $script:buildPacksExisted) {
        Remove-Item -Recurse -Force $buildPacks -ErrorAction SilentlyContinue
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
