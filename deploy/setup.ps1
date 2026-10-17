#requires -RunAsAdministrator
<#
    ArmouryDaemon setup — registers the logon task (highest privileges, no UAC prompt
    at logon) and starts the daemon now.

    Run from the folder that holds ArmouryDaemon.exe:
        powershell -ExecutionPolicy Bypass -File .\setup.ps1
#>

$ErrorActionPreference = 'Stop'
$exe = Join-Path $PSScriptRoot 'ArmouryDaemon.exe'
if (-not (Test-Path $exe)) { throw "ArmouryDaemon.exe not found next to setup.ps1" }

$taskName = 'ArmouryDaemon'
$user = "$env:USERDOMAIN\$env:USERNAME"

# highest privileges, interactive (needs the user session for the tray icon)
$action = New-ScheduledTaskAction -Execute $exe -WorkingDirectory $PSScriptRoot
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew

Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings -Force | Out-Null

Write-Host "Registered scheduled task '$taskName' for $user (runs at logon, highest privileges)."

# start it now (stop any daemon already running from another folder first)
Get-Process ArmouryDaemon -ErrorAction SilentlyContinue | Stop-Process -Force
Start-ScheduledTask -TaskName $taskName
Write-Host "Started. Look for the amber 'A' icon in the tray (hidden icons area)."
Write-Host "Config + logs: $env:APPDATA\ArmouryDaemon"
