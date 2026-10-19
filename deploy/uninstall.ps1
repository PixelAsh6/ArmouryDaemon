#requires -RunAsAdministrator
<#
    ArmouryDaemon uninstall - removes the autostart task and stops the daemon.

    The daemon is asked to quit over its named pipe first: "quit means close everything"
    (decision G10) - it reverts the packs (wallpaper, G-Helper mode, the Rainmeter layout)
    before it exits. A force kill would leave the machine in game mode.

    The config in %APPDATA%\ArmouryDaemon is kept unless you pass -PurgeConfig - and that
    folder holds the SteamGridDB API key, so -PurgeConfig deletes it as well.

    The install folder is left behind on purpose: delete it by hand when you are done.
#>
param([switch]$PurgeConfig)

$ErrorActionPreference = 'Stop'

$taskName = 'ArmouryDaemon'
$installDir = $PSScriptRoot


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


# ---------------------------------------------------------------------------
# the task
# ---------------------------------------------------------------------------

if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
    Write-Host "Removed scheduled task '$taskName'."
} else {
    Write-Host "No scheduled task named '$taskName'."
}

# the old project's task from an earlier install, if it is still registered
if (Get-ScheduledTask -TaskName 'Armoury Daemon' -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName 'Armoury Daemon' -Confirm:$false
    Write-Host "Removed the leftover legacy task '\Armoury Daemon'."
}


# ---------------------------------------------------------------------------
# the daemon
# ---------------------------------------------------------------------------

# A clean quit first: the daemon reverts the packs (wallpaper, G-Helper mode, the Rainmeter
# layout) before it exits. Only when that does not work is it killed - a force kill leaves the
# machine in game mode until the daemon is started again.
$running = @(Get-DaemonProcesses)
if ($running.Count -eq 0) {
    Write-Host 'The daemon is not running.'
} else {
    Write-Host ("Stopping the daemon (pid {0})." -f (($running | ForEach-Object { $_.Id }) -join ', '))

    $quit = $false
    if (Request-DaemonQuit) {
        Write-Host 'Asked it to quit over the pipe (it reverts the packs before it exits).'
        # the daemon waits up to 15 s for the revert, so give it a little more than that
        $deadline = (Get-Date).AddSeconds(20)
        while ((Get-Date) -lt $deadline) {
            $left = @(Get-DaemonProcesses)
            if ($left.Count -eq 0) { $quit = $true; break }
            if (@($left | Where-Object { $_.Main }).Count -eq 0) { $quit = $true; break }
            Start-Sleep -Milliseconds 250
        }
        if ($quit) { Write-Host 'It quit on its own - the packs are back to normal.' }
        else { Write-Warning 'It did not quit within 20 s.' }
    } else {
        Write-Warning 'The daemon pipe could not be reached (the daemon is not running, or it is a different install) - killing it instead.'
    }

    foreach ($p in @(Get-DaemonProcesses)) {
        try {
            Stop-Process -Id $p.Id -Force -ErrorAction Stop
            Write-Host "Killed pid $($p.Id) ($($p.Path))."
        } catch {
            Write-Warning "Could not kill pid $($p.Id): $($_.Exception.Message)"
        }
    }
}


# ---------------------------------------------------------------------------
# the config
# ---------------------------------------------------------------------------

if ($PurgeConfig) {
    Remove-Item "$env:APPDATA\ArmouryDaemon" -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Deleted config + logs in $env:APPDATA\ArmouryDaemon."
    Write-Host "That folder held the SteamGridDB API key and the cached cover art - you will have to paste the key again in Settings."
} else {
    Write-Host "Kept config + logs in $env:APPDATA\ArmouryDaemon (use -PurgeConfig to delete)."
    Write-Host "The SteamGridDB API key stays in that folder."
}

Write-Host "The install folder $installDir is left behind on purpose - delete it by hand when you are done."
