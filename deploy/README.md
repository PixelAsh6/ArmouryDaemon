# ArmouryDaemon

A small always-on Windows daemon that reacts to the apps you care about.

- **Tracked apps** — pick them from your running windows or browse for an exe (matched by exe name).
- **Packs** — Wallpaper Engine, G-Helper and Rainmeter. Each has exactly one dropdown: what it
  should do while a tracked app runs. When the app closes, everything reverts to the state it was
  in before (after a cooldown you choose; reopening cancels the revert).
- **Killswitch** — Scroll Lock hides + mutes the running tracked apps; press again to restore.
- **Idle mode** — one click blacks out every screen and mutes all audio; any input brings it back
  (with an optional 3 h failsafe).
- **Pause** — stop all pack switching without uninstalling anything.

Everything is local. No telemetry, no network calls except the optional pack catalog.

## Install

1. Put `ArmouryDaemon.exe` + the `packs` folder in a permanent folder, e.g. `C:\Apps\ArmouryDaemon`.
2. In an **administrator** PowerShell, in that folder:

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\setup.ps1
   ```

   This registers a Scheduled Task that starts the daemon at logon **with highest privileges**, so
   there is no UAC prompt afterwards, and starts it immediately.

3. The tray icon is the amber **A**. Left-click it for the dashboard; right-click for the menu
   (Open dashboard / Pause / Toggle killswitch / Quit).

## Where things live

| What | Where |
|---|---|
| Program + packs | the folder you installed into |
| Config | `%APPDATA%\ArmouryDaemon\config.json` |
| Logs | `%APPDATA%\ArmouryDaemon\logs\daemon.log`, `ui.log` |
| Pack journal | `%APPDATA%\ArmouryDaemon\journal.json` |

## Updating

Quit from the tray menu, replace `ArmouryDaemon.exe` (and `packs`), start it again. The config
survives.

## Uninstall

```powershell
powershell -ExecutionPolicy Bypass -File .\uninstall.ps1            # keeps config
powershell -ExecutionPolicy Bypass -File .\uninstall.ps1 -PurgeConfig
```

## Writing a pack

Packs are data-only JSON (see `packs/*/pack.json`): a target process, exactly one dropdown, what
to capture before acting, and ordered steps to apply and to revert. Available mechanisms: `cli`,
`hotkey`, `rainmeter_bang`, `process_kill`, `process_start`; captures: `process_running`,
`target_path`, `json_file`. No pack can execute arbitrary code.
