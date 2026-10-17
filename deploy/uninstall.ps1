#requires -RunAsAdministrator
<#
    ArmouryDaemon uninstall — removes the autostart task and stops the daemon.
    Leaves your config (%APPDATA%\ArmouryDaemon) alone unless you pass -PurgeConfig.
#>
param([switch]$PurgeConfig)

$ErrorActionPreference = 'Stop'
$taskName = 'ArmouryDaemon'

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

Get-Process ArmouryDaemon -ErrorAction SilentlyContinue | Stop-Process -Force
Write-Host "Stopped the daemon."

if ($PurgeConfig) {
    Remove-Item "$env:APPDATA\ArmouryDaemon" -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Deleted config + logs."
} else {
    Write-Host "Kept config + logs in $env:APPDATA\ArmouryDaemon (use -PurgeConfig to delete)."
}

Write-Host "You can now delete this folder."
