# ArmouryDaemon

A small always-on Windows daemon that reacts to the apps you care about.

This document describes **ArmouryDaemon 1.1.6** and is the reference for the scripted install,
the exact ACL, updating and uninstalling. The overview and the lone-exe install are in the
[README](../README.md).

- **Tracked apps** - pick them from your running windows or browse for an exe (matched by exe name).
- **Packs** - Wallpaper Engine, G-Helper and Rainmeter. A pack may expose up to four dropdowns
  (what it should do while a tracked app runs, and how); Wallpaper Engine ships with two
  (`action` and `binary`). When the app closes, everything reverts to the state it was in before
  (after the `Cooldown` lockout you choose in Settings; a quick reopen keeps the packs
  applied).
- **Killswitch** - **End** hides + mutes the running tracked apps; press again to restore. The key
  still reaches the app you are typing in (it is a low-level keyboard hook, not a registered hotkey).
- **Idle mode** - **Home** blacks out every screen and mutes all audio; a **keypress** brings it
  back (with an optional 3 h failsafe). The wake is **keyboard-only** by design: the mouse
  deliberately does not end an idle session, so a nudged mouse cannot do it by accident. While
  idle is up, pack switching is cancelled: tracked apps are
  treated as ordinary apps, and the packs are applied again when you wake it with a tracked app
  still running.
- **Pause** - stop all pack switching without uninstalling anything.
- **Console band** - a launcher strip along the bottom of the screen with the cover art of your
  tracked and pinned apps. Open it from the tray, let it pop up on a controller connect, or keep
  it out of the way. A game that does not start keeps the band open and shows you why.

Everything is local. No telemetry. The only network traffic is the optional pack catalog and the
cover art (Steam, or SteamGridDB when you set a key).

## Install

The executable can install itself - the [README](../README.md) has that path. This is the
scripted one: `setup.ps1` registers the same scheduled task, writes the checksum sidecar and
applies the folder ACL, and it needs the `packs` folder next to the executable.

The two registrations agree in every field that affects the daemon's behaviour. The task XML
itself differs in two fields that change nothing: the script writes task schema `1.3` where the
daemon's own registration writes `1.2`, and only the script sets `UseUnifiedSchedulingEngine`.

The executable targets the **x86-64 v3** level and needs AVX2 - Intel Haswell (2013) and later,
AMD Excavator (2015) and later; see [Building from source](../README.md#building-from-source).

1. Put `ArmouryDaemon.exe` + the `packs` folder in a permanent folder, e.g. `C:\Apps\ArmouryDaemon`.
2. In an **administrator** PowerShell, in that folder:

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\setup.ps1
   ```

   This registers a Scheduled Task that starts the daemon at logon and when Windows resumes from
   sleep **with highest privileges**, so there is no UAC prompt afterwards, and starts it
   immediately. It also removes the leftover disabled `\Armoury Daemon` task from an older install
   if one is still registered.

   The script locks the install folder down after it registers the task: Administrators and
   SYSTEM keep full control, the account you are logged in with keeps *modify* (the daemon writes
   the `packs` folder there), everybody else gets **read and execute only**. Without that, any
   other local account could swap the exe and inherit the elevated task at the next logon.
   The task is registered **first** on purpose: a failing ACL (or a failing hash sidecar) only
   warns and the script carries on to start the daemon - it can no longer leave the daemon down.

   The applied ACL is exactly: Administrators **full**, SYSTEM **full**, the account you are
   logged in with **modify**, `Authenticated Users` and `Users` **read and execute only** - the
   permissions inherited from `C:\` are removed, so nothing outside that list is left over. Check
   it any time with `(Get-Acl <install folder>).Access`.

   It also records the SHA256 of `ArmouryDaemon.exe` in `ArmouryDaemon.exe.sha256` and in the
   description of the scheduled task, so a swapped binary is detectable. To check it later, compare
   the sidecar with a fresh hash. The script computes it through .NET, because `Get-FileHash` is
   not available in a Windows PowerShell 5.1 session on this machine (the global `PSModulePath`
   puts the PowerShell 7 modules folder first, and the PS7 `Microsoft.PowerShell.Utility` exports
   nothing):

   ```powershell
   $sha = [System.Security.Cryptography.SHA256]::Create(); $fs = [System.IO.File]::OpenRead('<install folder>\ArmouryDaemon.exe'); $h = ([System.BitConverter]::ToString($sha.ComputeHash($fs)) -replace '-', '').ToLowerInvariant(); $fs.Dispose(); $sha.Dispose(); $h
   ```

3. The tray icon is a **purple squircle with a bold white A**. Left-click it for the dashboard;
   right-click for the menu (Open dashboard / Pause all packs / Idle mode / Toggle killswitch /
   Console mode / Quit).

## Where things live

| What | Where |
|---|---|
| Program + packs | the folder you installed into |
| Config | `%APPDATA%\ArmouryDaemon\config.json` |
| Logs | `%APPDATA%\ArmouryDaemon\logs\daemon.log`, `ui.log`, `console.log` |
| Cover art cache | `%APPDATA%\ArmouryDaemon\art` |
| Pack journal | `%APPDATA%\ArmouryDaemon\journal.json` |

Each log is rolled at 512 KiB. Three files are kept in total: the current one, then
`daemon.log.1` and `daemon.log.2`; the oldest is dropped.

## Cover art

The console band shows the cover art of your apps. Games Steam knows get it from the Steam CDN,
the rest from SteamGridDB - for that you paste an API key into Settings -> **SteamGridDB**. The
key is stored in the config and never shown in the clear. Without a key the band falls back to
the exe icon and then to a monogram.

## The pack catalog

The catalogue is a window of its own: Settings -> **Install packs** opens it, and it fetches the
manifest when it opens and lists every pack in it. Each row carries a **Download**, an **Update**
or a *vX installed* label; **Download** and **Update** fetch the pack, check it against the hash
and the size in the manifest and hand it to the daemon, which validates it and reloads. The
**Local pack** button opens the file picker and installs a `pack.json` from disk.

The catalog is `https://github.com/PixelAsh6/ArmouryDaemon_Packs` (MIT): `catalog/index.json` plus
one `packs/<id>/pack.json` per pack. Two things to know about it:

- **A fresh publish takes up to five minutes to show up.** `raw.githubusercontent.com` caches the
  `main` branch for up to five minutes, so *Check for updates* can still show the previous version
  for that long, and a pack file fetched from the stale copy does not match the manifest's hash -
  the install is refused. That is on purpose: nothing is installed that cannot be verified. The
  repository itself is never stale, only that cached view lags.
- **The manifest's hash and size must describe the published bytes.** Hash the file as GitHub
  serves it (`git cat-file blob HEAD:<path>` in the catalog repo), not as it sits in a Windows
  working tree: `core.autocrlf=true` turns LF into CRLF, the two sizes differ, and the install then
  fails on the size check. The catalog repo keeps its files LF with a `.gitattributes` rule
  (`*.json text eol=lf`).

## Updating

The running exe is locked, so it cannot be overwritten while the daemon is up - and `setup.ps1`
cannot replace it either: the script installs and starts, it never copies a binary. The order is
therefore fixed.

1. **Stop the daemon.** Right-click the purple squircle in the tray -> **Quit**. That reverts the
   packs first, so the wallpaper, the G-Helper mode and the Rainmeter layout are back to normal
   before the daemon goes away. A script can ask it to quit over the pipe instead - that is what
   `setup.ps1` and `uninstall.ps1` do - but the pipe is only reachable from an elevated shell.

2. **Copy the new files into the install folder** (e.g. `C:\Apps\ArmouryDaemon`): the new
   `ArmouryDaemon.exe`, the `packs\` folder and the three files from `deploy\` (`setup.ps1`,
   `uninstall.ps1`, `README.md`).

   ```powershell
   Copy-Item -Path 'D:\Armoury Daemon\target\release\ArmouryDaemon.exe' -Destination 'C:\Apps\ArmouryDaemon\' -Force
   Copy-Item -Path 'D:\Armoury Daemon\packs\*' -Destination 'C:\Apps\ArmouryDaemon\packs\' -Recurse -Force
   Copy-Item -Path 'D:\Armoury Daemon\deploy\setup.ps1','D:\Armoury Daemon\deploy\uninstall.ps1','D:\Armoury Daemon\deploy\README.md' -Destination 'C:\Apps\ArmouryDaemon\' -Force
   ```

   The copy has to happen **after** the daemon is stopped - while it runs the exe is locked and
   the copy fails with a sharing violation.

   The exe you are copying is the one in `target\release\`, and that is the **x86-64 v3** build:
   `.cargo/config.toml` adds `-C target-cpu=x86-64-v3` - a **fixed** target (AVX2, BMI1, BMI2,
   F16C, FMA, LZCNT, MOVBE, XSAVE), not the CPU of the machine that built it. It needs **AVX2** -
   Intel Haswell (2013) and later, AMD Excavator (2015) and later - and the release publishes
   this same build, so **you do not have to build it again on a new PC**: the only machine that
   cannot run it is one older than Haswell or Excavator. See
   [Building from source](../README.md#building-from-source).

3. **Run `setup.ps1` in the install folder, from an elevated PowerShell:**

   ```powershell
   cd C:\Apps\ArmouryDaemon
   powershell -ExecutionPolicy Bypass -File .\setup.ps1
   ```

   It has to be **elevated** (it registers the task with highest privileges and it sets the
   folder ACL), and it has to run **after** the copy - it cannot replace a locked running exe
   itself.

   The daemon is already stopped, so `setup.ps1` finds nothing to stop and then: writes
   `ArmouryDaemon.exe.sha256`, registers the task (logon + resume from sleep, highest
   privileges), applies the folder ACL and starts it. The order is deliberate - the task is
   registered before anything that could fail, and a failing ACL or a failing hash sidecar only
   warns. At the end the script checks that the daemon really came up and prints the final state
   (task / sidecar / ACL / running process).

The config survives an update.

## Uninstall

```powershell
powershell -ExecutionPolicy Bypass -File .\uninstall.ps1            # keeps config
powershell -ExecutionPolicy Bypass -File .\uninstall.ps1 -PurgeConfig
```

The uninstall asks the daemon to quit over its pipe first, so the packs are reverted instead of
being left in game mode. `-PurgeConfig` deletes `%APPDATA%\ArmouryDaemon` completely - that holds
your settings **and the SteamGridDB API key**, so you have to paste the key again in Settings if
you want the cover art back. The install folder itself is left behind on purpose: delete it by
hand when you are done.

## Writing a pack

Packs are data (see `packs/*/pack.json`): a target process, up to four dropdowns, what to capture
before acting, and ordered steps to apply and to revert. Available mechanisms: `cli`, `hotkey`,
`process_kill`, `process_start`, `registry_set`; captures: `process_running`, `target_path`,
`json_file`.

A spec-2 pack may carry a **script**: a small Rhai program that the daemon interprets against a
whitelisted host API, with an operation limit and a 10 s budget. The script is data, not compiled
code - it cannot reach outside the allowed folders - but it is not "no code" either.

What the daemon can run for a script: `ctx.log`, `ctx.capture` / `ctx.captured`, `ctx.setting`,
`ctx.target_name` / `ctx.target_exe` / `ctx.target_dir`, `ctx.run_cli`, `ctx.hotkey`,
**`ctx.window_message(class, title, payload)`**, `ctx.sleep`, `ctx.process_running` /
`ctx.process_kill` / `ctx.process_start`, `ctx.read_file` / `ctx.write_file` / `ctx.read_json` /
`ctx.read_ini`. `ctx.window_message` is the generic one: it finds a window of another app by class
and/or title (an empty field matches any) and sends it the payload as a `WM_COPYDATA` message, with
a 2 s timeout so a hung app cannot block the daemon. That is how the shipped Rainmeter pack loads
a layout - the bang and the window class are pack data, not daemon knowledge.

The **Local pack** button in the **Install packs** window opens the file picker and copies a
`pack.json` into this folder's `packs\<id>\` (the daemon validates it first and reloads). The
Settings window's **Open packs folder** button opens the folder itself, and **Remove** next to an
installed pack deletes it again.

## The self-test

`tools/daemon-selftest.ps1` drives a throw-away daemon instance (its own pipe, config folder and
single-instance guard) through the whole engine and prints one line per check - 84 of them. It
needs no Wallpaper Engine, G-Helper or Rainmeter, but it does need an **elevated** shell: the
executable carries a manifest that asks for administrator, so the runs it starts would raise a
UAC prompt. A harness that is not elevated **skips** the command-line probes and prints why.

```powershell
powershell -ExecutionPolicy Bypass -File tools\daemon-selftest.ps1
```

It refuses to run against an installed copy: the daemon resolves its packs folder to the first
folder that already has one, and the install/remove checks would overwrite and then delete a pack
of the running installation. Only an exe inside the repository is allowed.
