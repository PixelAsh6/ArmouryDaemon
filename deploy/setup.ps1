#requires -RunAsAdministrator
<#
    ArmouryDaemon setup — registers the autostart task (highest privileges, no UAC prompt) and
    starts the daemon now.

    Triggers:
      * at logon
      * when Windows resumes from sleep/hibernate (Kernel-Power event 107), if it can be built

    The task runs the exe with `--autostart`: if a daemon is already up (for example the logon
    trigger already ran), that second process exits silently — no dashboard pops up on resume.

    Run from the folder that holds ArmouryDaemon.exe:
        powershell -ExecutionPolicy Bypass -File .\setup.ps1
#>

$ErrorActionPreference = 'Stop'
$exe = Join-Path $PSScriptRoot 'ArmouryDaemon.exe'
if (-not (Test-Path $exe)) { throw "ArmouryDaemon.exe not found next to setup.ps1" }

$taskName = 'ArmouryDaemon'
$user = "$env:USERDOMAIN\$env:USERNAME"

# highest privileges, interactive (needs the user session for the tray icon)
$action = New-ScheduledTaskAction -Execute $exe -Argument '--autostart' -WorkingDirectory $PSScriptRoot
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

Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $triggers `
    -Principal $principal -Settings $settings -Force | Out-Null

Write-Host "Registered scheduled task '$taskName' for $user (logon + resume, highest privileges)."

# the old project's task, if it is still lying around (disabled) from an earlier install
if (Get-ScheduledTask -TaskName 'Armoury Daemon' -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName 'Armoury Daemon' -Confirm:$false
    Write-Host "Removed the leftover legacy task '\Armoury Daemon'."
}

# start it now (stop any daemon already running from another folder first)
Get-Process ArmouryDaemon -ErrorAction SilentlyContinue | Stop-Process -Force
Start-ScheduledTask -TaskName $taskName
Write-Host "Started. Look for the amber 'A' icon in the tray (hidden icons area)."
Write-Host "Config + logs: $env:APPDATA\ArmouryDaemon"
