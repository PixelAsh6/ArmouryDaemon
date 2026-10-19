#requires -RunAsAdministrator
<#
    ArmouryDaemon setup - registers the autostart task (highest privileges, no UAC prompt) and
    starts the daemon now.

    Triggers:
      * at logon
      * when Windows resumes from sleep/hibernate (Kernel-Power event 107), if it can be built

    The task runs the exe with `--autostart`: if a daemon is already up (for example the logon
    trigger already ran), that second process exits silently - no dashboard pops up on resume.

    The order of the steps is deliberate, so that nothing can leave the daemon down:
      1. the hash sidecar          - warns, never aborts
      2. the scheduled task         - the one blocker: if this fails the script stops
      3. the folder ACL             - warns, never aborts (it runs after the task is registered)
      4. stop the daemon            - clean quit over the pipe, so the packs revert
      5. start the task and verify  - prints the final state

    Run from the folder that holds ArmouryDaemon.exe:
        powershell -ExecutionPolicy Bypass -File .\setup.ps1
#>

$ErrorActionPreference = 'Stop'

$installDir = $PSScriptRoot
$exe = Join-Path $installDir 'ArmouryDaemon.exe'
$packsDir = Join-Path $installDir 'packs'
$taskName = 'ArmouryDaemon'
$user = "$env:USERDOMAIN\$env:USERNAME"


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

# Every ArmouryDaemon process, with the folder it runs from. The daemon and its --ui and
# --console children share the exe image, so the path is what tells the installs apart.
function Get-DaemonProcesses([string]$folder = '') {
    $prefix = ''
    if ($folder) { $prefix = $folder.TrimEnd('\') + '\' }
    $list = @()
    try {
        foreach ($p in @(Get-CimInstance -ClassName Win32_Process -Filter "Name='ArmouryDaemon.exe'" -ErrorAction Stop)) {
            $path = [string]$p.ExecutablePath
            if (-not $path) { continue }
            if ($prefix -and -not $path.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) { continue }
            $command = [string]$p.CommandLine
            $list += [pscustomobject]@{
                Id      = [int]$p.ProcessId
                Path    = $path
                Command = $command
                Main    = ($command -notmatch '--(ui|console)')
            }
        }
        return $list
    } catch {
        Write-Verbose "CIM is not available ($($_.Exception.Message)); falling back to Get-Process."
    }
    foreach ($p in @(Get-Process -Name 'ArmouryDaemon' -ErrorAction SilentlyContinue)) {
        $path = ''
        try { $path = $p.Path } catch { $path = '' }
        if (-not $path) { continue }
        if ($prefix -and -not $path.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) { continue }
        $list += [pscustomobject]@{
            Id      = $p.Id
            Path    = $path
            Command = ''
            Main    = $true
        }
    }
    return $list
}

# Ask the daemon to quit over its named pipe (one JSON object per line).
function Request-DaemonQuit {
    try {
        $pipe = New-Object System.IO.Pipes.NamedPipeClientStream('.', 'armoury-daemon.pipe', [System.IO.Pipes.PipeDirection]::InOut)
        try {
            $pipe.Connect(2000)
            $writer = New-Object System.IO.StreamWriter($pipe)
            $writer.AutoFlush = $true
            $writer.WriteLine('{"type":"quit"}')
            $writer.Dispose()
        } finally {
            $pipe.Dispose()
        }
        return $true
    } catch {
        Write-Verbose "the daemon pipe is not reachable: $($_.Exception.Message)"
        return $false
    }
}

# Stop the daemon that runs from $folder: a clean quit first, the hard kill only as a fallback.
function Stop-DaemonInFolder([string]$folder) {
    $running = @(Get-DaemonProcesses $folder)
    if ($running.Count -eq 0) { return $false }

    Write-Host ("Stopping the daemon in {0} (pid {1})." -f $folder, (($running | ForEach-Object { $_.Id }) -join ', '))

    $quit = $false
    if (Request-DaemonQuit) {
        Write-Host 'Asked it to quit over the pipe (it reverts the packs before it exits).'
        # the daemon waits up to 15 s for the revert, so give it a little more than that
        $deadline = (Get-Date).AddSeconds(20)
        while ((Get-Date) -lt $deadline) {
            $left = @(Get-DaemonProcesses $folder)
            if ($left.Count -eq 0) { $quit = $true; break }
            if (@($left | Where-Object { $_.Main }).Count -eq 0) { $quit = $true; break }
            Start-Sleep -Milliseconds 250
        }
        if ($quit) { Write-Host 'It quit on its own - the packs are back to normal.' }
        else { Write-Warning 'It did not quit within 20 s.' }
    } else {
        Write-Warning 'The daemon pipe could not be reached (no daemon, or a different install).'
    }

    # whatever is left: the daemon itself, or an orphaned --ui/--console child
    foreach ($p in @(Get-DaemonProcesses $folder)) {
        try {
            Stop-Process -Id $p.Id -Force -ErrorAction Stop
            Write-Host "Killed pid $($p.Id)."
        } catch {
            Write-Warning "Could not kill pid $($p.Id): $($_.Exception.Message)"
        }
    }
    return $true
}

# The SHA256 of a file as lowercase hex, computed through .NET.
#
# Get-FileHash is NOT used on purpose: the global PSModulePath on this machine puts the PowerShell
# 7 Modules folder ahead of the Windows PowerShell one, so a powershell.exe (5.1) session
# autoloads the PS7 Microsoft.PowerShell.Utility - which exports nothing - and the cmdlet is not
# there at all (it stays missing even after Import-Module "succeeds"). The .NET call does not care
# about the module path, so it works in every session.
function Get-Sha256Hex([string]$path) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $stream = $null
    try {
        $stream = [System.IO.File]::Open($path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
        $bytes = $sha.ComputeHash($stream)
    } finally {
        if ($stream) { $stream.Dispose() }
        $sha.Dispose()
    }
    return ([System.BitConverter]::ToString($bytes) -replace '-', '').ToLowerInvariant()
}


# ---------------------------------------------------------------------------
# what is being installed
# ---------------------------------------------------------------------------

if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    throw "ArmouryDaemon.exe not found next to setup.ps1 ($installDir)"
}
if (-not (Test-Path -LiteralPath $packsDir -PathType Container)) {
    throw "the 'packs' folder is missing next to ArmouryDaemon.exe ($packsDir) - copy it from the release first"
}

# The exe runs as Administrator at every logon, so its hash is recorded: a later mismatch
# (a swapped binary) has to be detectable. The hash goes into a sidecar file next to the exe
# and into the description of the scheduled task.
#
# This step must NOT abort the script. A missing sidecar is a security gap worth a loud
# warning, but it is no reason to leave the daemon down: the task has to be registered and
# started whatever happens here.
$hash = ''
$hashFile = "$exe.sha256"
$sidecarOk = $false
try {
    $hash = Get-Sha256Hex $exe
    Set-Content -LiteralPath $hashFile -Value ("{0}  {1}" -f $hash, (Split-Path $exe -Leaf)) -Encoding ASCII -ErrorAction Stop
    $sidecarOk = $true
    Write-Host "Recorded the exe hash: $hash"
    Write-Host "          (also in $hashFile)"
} catch {
    Write-Warning "The hash sidecar $hashFile was NOT written: $($_.Exception.Message)"
    Write-Warning "The exe hash is not recorded, so a swapped binary would not be detectable. Write it by hand later:"
    Write-Warning "  `$sha = [System.Security.Cryptography.SHA256]::Create(); `$fs = [System.IO.File]::OpenRead('$exe'); `$h = ([System.BitConverter]::ToString(`$sha.ComputeHash(`$fs)) -replace '-', '').ToLowerInvariant(); `$fs.Dispose(); `$sha.Dispose()"
    Write-Warning "  Set-Content -Path '$hashFile' -Value (`$h + '  ArmouryDaemon.exe') -Encoding ASCII"
}


# ---------------------------------------------------------------------------
# the scheduled task
# ---------------------------------------------------------------------------

# highest privileges, interactive (needs the user session for the tray icon)
$action = New-ScheduledTaskAction -Execute $exe -Argument '--autostart' -WorkingDirectory $installDir
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew

$triggers = @(New-ScheduledTaskTrigger -AtLogOn -User $user)

# A "resume from sleep" trigger is an event trigger on Kernel-Power 107 (Task Scheduler has no
# dedicated resume trigger). Built through the Task Scheduler CIM classes; skipped when the
# machine does not offer them.
try {
    $class = Get-CimClass -Namespace root/Microsoft/Windows/TaskScheduler -ClassName MSFT_TaskEventTrigger
    $resume = New-CimInstance -CimClass $class -ClientOnly
    $resume.Enabled = $true
    $resume.Subscription = '<QueryList><Query Id="0" Path="System"><Select Path="System">*[System[Provider[@Name=''Microsoft-Windows-Kernel-Power''] and EventID=107]]</Select></Query></QueryList>'
    $triggers += $resume
    Write-Host "Resume-from-sleep trigger added (Kernel-Power event 107)."
} catch {
    Write-Warning "Could not build the resume trigger ($($_.Exception.Message)); logon only."
}

if ($sidecarOk) {
    $description = "ArmouryDaemon autostart. exe SHA256 $hash (see ArmouryDaemon.exe.sha256)."
} else {
    $description = "ArmouryDaemon autostart. exe SHA256 not recorded (the hash sidecar could not be written)."
}

# This is the one step that has to succeed. If the task cannot be registered the daemon does
# not come up at logon or resume, so the script stops here - loudly, and before anything that
# could take the running daemon down.
try {
    Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $triggers `
        -Principal $principal -Settings $settings -Description $description -Force -ErrorAction Stop | Out-Null
} catch {
    throw ("Could not register the scheduled task '{0}' ({1}). This is a blocker: without the task the daemon does not come up at logon or resume. The running daemon was left alone and nothing was stopped." -f $taskName, $_.Exception.Message)
}

Write-Host "Registered scheduled task '$taskName' for $user (logon + resume, highest privileges)."

# the old project's task, if it is still lying around (disabled) from an earlier install
if (Get-ScheduledTask -TaskName 'Armoury Daemon' -ErrorAction SilentlyContinue) {
    try {
        Unregister-ScheduledTask -TaskName 'Armoury Daemon' -Confirm:$false -ErrorAction Stop
        Write-Host "Removed the leftover legacy task '\Armoury Daemon'."
    } catch {
        Write-Warning "Could not remove the leftover legacy task '\Armoury Daemon': $($_.Exception.Message)"
    }
}


# ---------------------------------------------------------------------------
# lock the install folder down
# ---------------------------------------------------------------------------

# The task starts this exe as Administrator at every logon and every resume. Without a DACL
# of its own the folder inherits "Authenticated Users: Modify" from C:\, so any other local
# account could replace the exe and get code execution as Administrator at the next logon.
# The interactive user keeps Modify (the daemon writes <install>\packs), everybody else gets
# read + execute only.
#
# This runs AFTER the task is registered, and it must not abort the script. A folder that
# stays writable is a security gap, not a reason to leave the daemon down, so a failure here
# warns loudly and the script carries on to start the daemon.
$grants = @(
    'BUILTIN\Administrators:(OI)(CI)F'
    'NT AUTHORITY\SYSTEM:(OI)(CI)F'
    "${user}:(OI)(CI)M"
    'BUILTIN\Users:(OI)(CI)RX'
    'NT AUTHORITY\Authenticated Users:(OI)(CI)RX'
)
$aclOk = $false
try {
    & icacls $installDir /inheritance:r | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls could not drop the inherited permissions of $installDir (exit code $LASTEXITCODE)" }
    & icacls $installDir /grant:r $grants | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls could not set the permissions of $installDir (exit code $LASTEXITCODE)" }
    $aclOk = $true
    Write-Host "Locked $installDir down: Administrators + SYSTEM full, $user modify, everybody else read + execute."
} catch {
    Write-Warning "The ACL of $installDir was NOT applied: $($_.Exception.Message)"
    Write-Warning "Until it is, the folder inherits 'Authenticated Users: Modify' from C:\ - any other local account could replace ArmouryDaemon.exe and inherit the elevated task at the next logon."
    Write-Warning "Fix it by hand - Administrators + SYSTEM full, $user modify, everybody else read and execute:"
    Write-Warning "  (Get-Acl '$installDir').Access"
}


# ---------------------------------------------------------------------------
# stop the daemon that runs from this folder
# ---------------------------------------------------------------------------

# "Quit means close everything" (decision G10): the daemon reverts the packs (wallpaper,
# G-Helper mode, the Rainmeter layout) before it exits, so it is asked to quit over its
# named pipe first. A force kill would leave the machine in game mode - on an upgrade too.
# Nothing running from this folder is the normal case on a fresh install and on an update
# (the daemon is stopped before the new exe is copied over), so it is a no-op, not an error.
$stopped = Stop-DaemonInFolder $installDir
if (-not $stopped) {
    Write-Host 'No daemon is running from this folder - nothing to stop.'
}

# a daemon of another install is left alone (only one instance may run)
$prefix = $installDir.TrimEnd('\') + '\'
$others = @(Get-DaemonProcesses | Where-Object { -not $_.Path.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase) })
if ($others.Count -gt 0) {
    Write-Warning ("Another ArmouryDaemon is still running ({0}). It was left alone - stop it by hand, the new one will not start next to it." -f (($others | ForEach-Object { "pid $($_.Id) in $($_.Path)" }) -join ', '))
}


# ---------------------------------------------------------------------------
# start it now
# ---------------------------------------------------------------------------

# The task is registered by now, so a failed start is not fatal: the daemon comes up at the
# next logon or resume. It still has to be loud - a daemon that did not start is the one thing
# this whole script is for.
try {
    Start-ScheduledTask -TaskName $taskName -ErrorAction Stop
    Write-Host "Started the scheduled task '$taskName'."
} catch {
    Write-Warning "Could not start the task '$taskName': $($_.Exception.Message)"
    Write-Warning "The task is registered, so it comes up at the next logon or resume - but check the task result:"
    Write-Warning "  (Get-ScheduledTaskInfo -TaskName '$taskName').LastTaskResult"
}

# Verify that the daemon actually came up. Start-ScheduledTask is asynchronous and says
# nothing about the outcome, so the process is looked for directly.
$running = $null
$deadline = (Get-Date).AddSeconds(10)
do {
    Start-Sleep -Milliseconds 500
    $main = @(Get-DaemonProcesses $installDir | Where-Object { $_.Main })
    # the scheduled task starts it with --autostart; fall back to any main daemon process
    # when the command line cannot be read
    $running = @($main | Where-Object { $_.Command -match '--autostart' }) | Select-Object -First 1
    if (-not $running) { $running = $main | Select-Object -First 1 }
} while (-not $running -and (Get-Date) -lt $deadline)

if ($running) {
    Write-Host ("The daemon is running: pid {0} ({1})." -f $running.Id, $running.Path)
} else {
    Write-Warning "The daemon is NOT running: no ArmouryDaemon.exe --autostart process from $installDir."
    Write-Warning "The task '$taskName' is registered, so it comes up at the next logon or resume - but check now:"
    Write-Warning "  the task result: (Get-ScheduledTaskInfo -TaskName '$taskName').LastTaskResult"
    Write-Warning "  the daemon log:  $env:APPDATA\ArmouryDaemon\logs\daemon.log"
    Write-Warning "  the scheduler log: Get-WinEvent -LogName 'Microsoft-Windows-TaskScheduler/Operational' -MaxEvents 20"
    Write-Warning "  a daemon of another install may still be running (only one instance may run)"
}


# ---------------------------------------------------------------------------
# final state
# ---------------------------------------------------------------------------

if ($sidecarOk) { $sidecarState = "written - $hashFile" } else { $sidecarState = 'NOT written - see the warning above' }
if ($aclOk) { $aclState = "applied - $installDir" } else { $aclState = 'NOT applied - see the warning above' }
if ($running) { $daemonState = "yes - pid $($running.Id)" } else { $daemonState = 'NO - the task is registered, see the warning above' }

Write-Host ''
Write-Host 'Final state:'
Write-Host "  Task registered : yes - '$taskName' (logon + resume from sleep, highest privileges)"
Write-Host "  Hash sidecar    : $sidecarState"
Write-Host "  Folder ACL      : $aclState"
Write-Host "  Daemon running  : $daemonState"
Write-Host "Look for the purple squircle with a white 'A' in the tray (hidden icons area)."
Write-Host "Config + logs: $env:APPDATA\ArmouryDaemon"

# Only the daemon itself not coming up is a failure of the script. A missing ACL or sidecar
# is a warning: the daemon runs, and the gap is named above.
if (-not $running) { exit 1 }
