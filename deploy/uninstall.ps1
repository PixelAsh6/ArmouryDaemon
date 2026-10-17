#requires -RunAsAdministrator
<#
    ArmouryDaemon uninstall — removes the logon task and stops the daemon.
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

Get-Process ArmouryDaemon -ErrorAction SilentlyContinue | Stop-Process -Force
Write-Host "Stopped the daemon."

if ($PurgeConfig) {
    Remove-Item "$env:APPDATA\ArmouryDaemon" -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Deleted config + logs."
} else {
    Write-Host "Kept config + logs in $env:APPDATA\ArmouryDaemon (use -PurgeConfig to delete)."
}

Write-Host "You can now delete this folder."
