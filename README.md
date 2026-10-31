# ArmouryDaemon

[![Licence: MIT](https://img.shields.io/badge/Licence-MIT-yellow.svg)](LICENSE)
[![Version](https://img.shields.io/badge/version-1.3.2-blue.svg)](Cargo.toml)

A resident Windows daemon that watches the applications you track and switches the rest of your
machine around them. While a tracked app runs, Rainmeter, G-Helper and Wallpaper Engine get out
of the way; when it closes, everything goes back to how it was.

Windows 11 · Rust · MIT

## What it is

ArmouryDaemon is an always-on daemon with an on-demand GUI, in one executable. Once it is
installed it runs resident and elevated: the scheduled task starts it with the highest
privileges, so it comes up without a UAC prompt. The first manual launch does ask for
administrator, because the executable carries a manifest that says so.

You pick the apps you track. While one of them runs, the daemon applies **packs**. A pack says
what the other programs should do while your app is running, and it records what they were doing
so the daemon can put it back. When the app closes, every pack reverts to the state captured
before it started.

Everything is local. There is no telemetry. The only network traffic is the optional pack
catalogue and the optional cover art.

## What it does

### Tracked applications

You choose the apps to track from the running windows or by browsing for an executable. They are
matched by **exe file name**, case-insensitively, so a game installed anywhere on disk is the
same tracked app.

The watcher walks the process table once a second, diffs the PID set and waits on the matching
process handles. An exit is reported by the kernel, not inferred from a poll, and a PID that
comes back after its process died is treated as a new start rather than a pause. While **no
applications are tracked** the walk is skipped; the tracked list is still read every tick, so an
app added between two ticks is seen immediately rather than a tick late.

### Packs

A pack is **data, not code**. The daemon interprets it, and nothing a pack contains can run
arbitrary code. One `pack.json` carries the target process, up to four dropdowns, what to capture
before acting, and the ordered steps to apply and to revert.

There are three roles:

| Role | What it is |
|---|---|
| **pack** | Follows a tracked app: applied while one runs, reverted when the last one closes. |
| **mode** | A global on/off component with a trigger of its own. It acts on the daemon itself, through `daemon` steps such as `idle_show` or `killswitch_on`. |
| **helper** | A separate executable the daemon starts and then leaves alone. The console bar is the only one today. |

**Where to get them.** The catalogue repository,
**[PixelAsh6/ArmouryDaemon_Packs](https://github.com/PixelAsh6/ArmouryDaemon_Packs)**, publishes
three packs (Wallpaper Engine, G-Helper, Rainmeter) and three modules (the Console bar, idle
mode and the killswitch). Its README is the reference for the pack format, the six step
mechanisms, the capture vocabulary and the script host API.

**How to install one.** Settings → **Packs**, or:

```console
ArmouryDaemon.exe --catalog-install <id>
```

A pack may declare the daemon it needs (`"min_engine": "1.2.0"`). A daemon that is too old
refuses it by name and the pack never lands on disk:

```console
pack 'example-pack' (Example Pack) needs daemon 1.2.0 or newer; this daemon is 1.3.2
```

The shipped packs are fine: Rainmeter asks for `1.0.0`.

### The pack catalogue

The catalogue is its own window, titled **Packs**. It fetches the manifest when it opens and
lists every entry with its version and one of *Download*, *Update* or *vX installed*, plus
**Update all**, **Check for updates** and **Local pack**. **Local pack** opens the native file
picker and installs a `pack.json` from disk; it is the only way to install a local pack file.

Nothing unverified is installed. The manifest travels minisign-signed as
`index.json.minisig` and is checked against the keyring compiled into the executable **before**
it is parsed, so a manifest whose signature does not verify lists nothing at all. Each file is
then checked against its **size**, then its **SHA-256**, and only then validated as a pack and
installed. A pack the running daemon is too old for never appears as a download; its row shows
the reason instead.

The catalogue needs no API key. The manifest and the pack files are public files fetched from
the packs repository's raw URL. The SteamGridDB key in Settings is for the cover art only.

The fetch uses the Windows certificate store, not a bundled root list, and goes through the
desktop session's proxy. A machine with a corporate CA, or one whose TLS is intercepted, can
still reach the catalogue. Cover art shares the same client, so it shares the same root store
and connection pool.

### The lockout ("Cooldown")

The lockout is the settling window in Settings, 60 seconds by default. It holds a change in both
directions: while a window runs, no automatic change may touch the packs.

- A tracked app **starts** with no window running → the packs go to the applied state at once.
  This is the first launch of a session, the machine is free, and the change starts a window.
- A tracked app **starts inside a window** → the apply is held. The packs stay where they are,
  and go to the applied state when the window is over, if the app still runs then.
- The last tracked app **closes** → the revert is held too. The close starts a window if none is
  running, or keeps the one that is, and the revert lands when it ends.
- The app **starts again inside that window** → the held revert is cancelled. The machine is
  already in the state the app wants.
- When the window ends, the held change lands **only if it is still wanted**, and it lands
  **once**. That change starts a window of its own.
- A window that expires while the app still runs changes nothing and starts no new one.
- A change of the cooldown **re-times the window that is running**: it keeps its start and takes
  the new length, so it ends at `start + cooldown`. The held change lands at the new expiry.

The previous state is always captured on a settled machine, so a capture can never read the
daemon's own value back.

Manual actions bypass the lockout and change the packs at once: removing a tracked app, pausing,
and entering or leaving idle mode.

### The console bar

The launcher is a **module**, a helper the daemon starts, and both the catalogue and the
daemon's own UI call it the **Console bar**. Its own window holds the open
mode, the apps pinned to it and the SteamGridDB key its cover art uses. With the module not
installed there is no band, no row state and no key field.

Plug in a controller and the band opens over your tracked and pinned apps. The d-pad or left
stick scrolls, **A** launches, **B** closes, and unplugging the pad closes it too. A mouse click
launches and `Esc` closes.

The daemon does the launching, so a game started from the band triggers the packs exactly as if
you had opened it by hand. A Steam app is started the way Steam starts it
(`steam.exe -applaunch <appid>`); anything else is spawned directly. The daemon checks that the
process really came up, and a failed launch keeps the band open with the reason instead of
sliding away.

The band opens from the tray as well, and can be told to stay out of the way: **Always**, **On
pad connect** (the default) or **Never**.

### Global hotkeys

A key belongs to the module it drives. **Killswitch** ships with **End**, **idle mode** with
**Home**, and each module's own window records its own combination of up to four keys. The
daemon rebuilds the table from the installed modules, so a module that is not installed owns no
key and its combination reaches the app you are in.

Both keys run on a **low-level keyboard hook** (`WH_KEYBOARD_LL`), not `RegisterHotKey`. A
registered hotkey is swallowed by Windows and never reaches the foreground application, and End
and Home are used in every text field, list, IDE and game. The hook calls `CallNextHookEx`, so
the key still arrives everywhere else.

The killswitch keeps a ledger: the windows it hid and the audio sessions it muted, each with its
previous state. It puts back only what it changed, so a session you had muted yourself stays
muted.

### Idle mode

**Home** blackens every monitor with a topmost window and mutes every output device. The screens
carry a grey **IDLE — press anything to wake up** hint. Any keypress wakes it. The mouse
deliberately does not, so a nudged mouse cannot end a session. A failsafe quits idle mode after a
configurable number of minutes, 180 by default.

Muted devices are recorded in `idle.ledger` as they are changed, so a crash during an idle
session cannot leave the machine silent after a restart.

Idle mode is an overlay and a mute, nothing else. It does not revert the packs, freeze their
switching, or re-apply them when you wake it. A tracked app that starts behind the black screens
is handled exactly as it would be without them.

### Cover art

The band takes the best art it can find. Steam's own files come first: the real 600×900 library
capsule (`library_600x900_2x.jpg`) for the tile, and the widest art Steam has for the background
(`library_hero.jpg` is the 1920×620 hero). SteamGridDB is asked only when Steam has nothing,
which needs an API key. Without one, and without a Steam match, the band falls back to the exe
icon and then to a monogram.

The automatic pick is not final. The **≡** menu in the band's top-left corner has **Choose
artwork…**, which opens a picker with an **Icon** and a **Background** tab: the 2:3 grids for the
icon, the wide heroes for the background, each with its size and style underneath. The mouse and
a controller both work. Your choice stays until you pick **Use automatic again**.

### The command line

```console
ArmouryDaemon.exe                     # the resident daemon
ArmouryDaemon.exe --catalog-list      # list the catalogue and exit
ArmouryDaemon.exe --catalog-install <id>
ArmouryDaemon.exe --catalog-url <url> # read the catalogue from this manifest instead
ArmouryDaemon.exe --module-list       # print what is installed and exit
ArmouryDaemon.exe --version           # ArmouryDaemon 1.3.2
ArmouryDaemon.exe --help
```

`--catalog-url <url>` on its own lists that manifest. `--module-list` prints every installed pack,
mode and helper, one line each, sorted by id.

Three flags exist for the daemon itself and are not meant for the command line: `--ui` (the GUI
process), `--install` (register the autostart task and exit) and `--autostart` (start as the
scheduled task does, leaving a running instance alone).

An argument the binary does not know is an error: the message and the usage go to stderr, exit
code 2, and the daemon does not start. An unknown argument outranks `--version`, so
`--version --bogus` is an error rather than a version.

The release build is a **GUI-subsystem binary** (PE subsystem 2), so it attaches to the parent
console to print. `--version` writes `ArmouryDaemon 1.3.2` on stdout and exits 0. When stdout is
not a console the line also goes to stderr, which is why `2>&1 | Out-String` shows it twice.
`cmd.exe`'s own `>` can come out empty, because the process re-points the standard handles after
attaching, and PowerShell 7 does not wait for a GUI-subsystem child, so `$LASTEXITCODE` stays
empty after `& exe --version`.

The executable carries a manifest that asks for administrator, so `--version` and `--help` from a
non-elevated shell raise one UAC prompt. From an elevated shell they do not.

### The pack script host (spec 2 packs)

A pack may carry a small **Rhai** script in `script.apply` and `script.revert`. The daemon
compiles the source once per capability set and runs it in an interpreter that exposes only the
host functions the pack asked for: no `eval`, no imports, no ambient file or process access. A
call to a function of a capability the pack did not declare fails as an unknown function.

Every host call is written to the pack's log. A run gets a hard budget of 500 000 operations, a
64 KiB string limit and 10 seconds of wall clock. Writes and program launches are restricted to
`%APPDATA%`, `%LOCALAPPDATA%`, `%PROGRAMDATA%` and the target app's own folder.

The generic function is `ctx.window_message(class, title, payload)`: it finds a window of another
application by class and/or title and sends the payload as a `WM_COPYDATA` message, with a 2
second timeout so a hung application cannot block the daemon. That is how the Rainmeter pack
loads a layout: the bang and the window class are pack data, and the daemon knows nothing about
Rainmeter's protocol.

## What you see

There are no screenshots in this repository, so none are linked. The daemon is a set of small
undecorated windows plus a tray icon.

- **The tray icon** is a purple-magenta squircle with a bold white **A**, drawn procedurally
  rather than loaded from a file. It turns **red** while the killswitch is engaged. Left-click
  opens the dashboard; right-click opens the menu: Open dashboard / Pause all packs / Idle mode /
  toggle killswitch / Console bar / Quit. The pause item reads Resume all packs while paused.
- **The executable carries the same icon**, embedded as a PE resource, so Explorer, shortcuts and
  the pinned taskbar item show the squircle instead of the generic Windows icon.
- **The dashboard** opens undecorated in the bottom-right corner above the tray. It shows the
  state, version, uptime and pack count, then **Tracked apps** and **Packs**, with a status band
  for *paused* and *cooling down*. The cooling-down band names the app the window belongs to, and
  the cooldown row says a change is waiting behind the window. Its Packs section also carries the
  **Console bar** row, because the band is a module and not a pack.
- **The Settings window** opens to the left of the dashboard: **Cooldown**, the **Idle failsafe**
  switch with **Leave idle after**, every installed pack with its **Remove**, and the buttons
  **Packs**, **Open packs folder**, **Reveal logs** and **Open config folder**. A module keeps its
  own settings in its own window: the console bar's open mode, pinned apps and SteamGridDB key,
  and each mode's hotkey.
- **The Packs window** is the catalogue list, opened from Settings and titled Packs. Each row
  carries its description in a column of its own, and the window carries **Check for updates**,
  **Update all** and **Local pack**.
- **One window per pack**, titled `Pack: <id>`, showing all of its dropdowns and its version.
- **The console bar** is the wide, dark, always-on-top tile strip along the bottom of the screen.
- **The idle overlay** is the black screens idle mode puts up.

Windows open in a leftward chain: a new window goes immediately left of the current leftmost one,
bottom edges aligned, with a gap of exactly **5 physical pixels**. The gap is a literal pixel
count, so display scaling cannot change it; window sizes still scale with content and scale.

## Install

The daemon is not on any package registry. It is installed from a folder you choose.

The executable targets the **x86-64 v3** level and needs AVX2: Intel Haswell (2013) and later,
AMD Excavator (2015) and later. See [Building from source](#building-from-source).

### From the executable alone

1. **Download** `ArmouryDaemon.exe` from the
   [v1.3.2 release](https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.3.2) and put it in
   a folder you can write to, for example `C:\Apps\ArmouryDaemon`. The release carries two
   assets and nothing else: the executable and its `ArmouryDaemon.exe.sha256` sidecar, so a
   swapped binary is detectable.
2. **Run it.** Windows shows one consent prompt, because the manifest asks for administrator.
   Accept it. The daemon then:
   - creates the `packs\` folder next to itself if it is missing, since a fresh install has no
     packs until you install some;
   - registers its own scheduled task, at logon and whenever Windows resumes from sleep, with the
     highest privileges, so it comes back by itself;
   - **opens the dashboard** on the first run only. A resume from sleep never opens it.
3. **Settings → Packs** lists the catalogue with a **Download**, **Update** or *vX installed*
   action per row. It needs no API key. [The catalogue section](#the-pack-catalogue) covers the
   verification.

The packs and modules are not in the release. The catalogue is where they come from. A pack lands
in the `packs\` folder next to the executable, which is where your own packs go too. A module
lands in `modules\<id>\` as a record plus its executable.

### Or the scripted install

The release publishes no zip, so there is nothing to unpack. The scripted install lives in the
**`deploy\` folder of the repository**.

1. Put `ArmouryDaemon.exe` and the `deploy\` folder's `setup.ps1` in a permanent folder, for
   example `C:\Apps\ArmouryDaemon`.
2. In an **administrator** PowerShell, in that folder:

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\setup.ps1
   ```

   The script registers the same scheduled task and starts it immediately. It agrees with the
   task the executable writes for itself in every field that affects behaviour; the task XML
   differs in two fields that change nothing (task schema `1.3` where the daemon writes `1.2`,
   and only the script sets `UseUnifiedSchedulingEngine`). It writes the executable's SHA-256
   into the sidecar and into the task description, and applies an explicit ACL to the folder:
   Administrators and SYSTEM full control, your account modify (the daemon writes `packs` there),
   everybody else read and execute only.

   The task is registered first on purpose. A failing ACL or sidecar only warns and the script
   carries on, so the install cannot leave the daemon down.

3. The tray icon appears. Left-click it for the dashboard.

**[deploy/README.md](deploy/README.md)** has the exact ACL, the update order (the running
executable is locked, so stop → copy → run `setup.ps1`) and the uninstall.

## Using it

| What | Where |
|---|---|
| Dashboard | Left-click the tray icon |
| Settings | The gear in the dashboard's title bar |
| Packs | Settings → **Packs**, or the **packs** folder in the install folder |
| Tracked apps | The dashboard: the inline type-to-filter picker over running processes, and **Browse.** |
| Config | `%APPDATA%\ArmouryDaemon\config.json` |
| Logs | `%APPDATA%\ArmouryDaemon\logs\daemon.log`, `ui.log`, `console.log` |
| Cover art cache | `%APPDATA%\ArmouryDaemon\art` |
| Pack journal | `%APPDATA%\ArmouryDaemon\journal.json` |

A log rolls at 512 KiB and three files are kept: the current one, then `.1` and `.2`.

The daemon alone writes the config. A malformed config is never destroyed: its bytes are kept
beside it as `config.json.bad` (then `.bad.2` through `.bad.5`) and the daemon runs on the
defaults. A key this build does not recognise is kept and written back. A config written by a
newer build is never overwritten, and a newer `schema_version` makes every save refuse rather
than truncate the file.

## Building from source

You need the **MSVC Build Tools** (Visual Studio 2019 or 2022 both work) and a Rust toolchain:
edition 2024, `rust-version` 1.95.

```powershell
cargo build --release        # -> target\release\ArmouryDaemon.exe
cargo test                   # the unit tests, both crates and both binaries
powershell -ExecutionPolicy Bypass -File tools\daemon-selftest.ps1
```

The self-test starts a throw-away daemon instance with its own pipe, config folder and
single-instance guard, and prints one line per check. It needs no Wallpaper Engine, G-Helper or
Rainmeter. It refuses to run against an installed copy, because the install and remove checks
would overwrite a pack of the running installation.

Run it from an **elevated** PowerShell: the executable's manifest asks for administrator, so the
runs it starts would raise a UAC prompt. A self-test that is not elevated skips the command-line
probes and prints why. A skipped check is not a failed one.

**The published executable has a fixed target.** `.cargo/config.toml` sets
`-C target-cpu=x86-64-v3`, a fixed microarchitecture level rather than the CPU of the machine
that built it, so the build is identical everywhere and that is the build the release ships. The
level is AVX2, BMI1, BMI2, F16C, FMA, LZCNT, MOVBE and XSAVE on top of v2's CX16, POPCNT and
SSE4.2. Intel has had AVX2 since Haswell (2013); AMD only since Excavator (2015). AMD's
2013-2014 parts (Steamroller, Jaguar, Kabini, Temash) and anything before 2011 have no AVX2, so
the binary will not start there.

The same file also links the C runtime statically (`-C target-feature=+crt-static`), so the
executable imports no `VCRUNTIME140.dll` and starts on an image without the redistributable.

The release profile is `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`,
`strip = "debuginfo"`, `incremental = false`.

## Project layout

```text
Cargo.toml                  the workspace (two crates, one release profile)
crates/
  armoury-core/             config, protocol, status, engine, pack schema, logging, icon
  armoury/                  the two binaries: ArmouryDaemon and ArmouryConsole
packs/                      five pack.json files, one per shipped pack: wallpaper-engine,
                            g-helper and rainmeter (which follow a tracked app), and idle and
                            killswitch (mode packs with a hotkey trigger)
modules/console/            the console bar's archive, as the catalogue serves it
deploy/                     setup.ps1, uninstall.ps1, README.md
docs/                       the published modularity notes
release-notes/              one file per release
tools/daemon-selftest.ps1   the self-test, with its fixtures/
```

`crates/armoury` is the daemon: tray, pipe server, watcher, hotkey hook, killswitch, idle
overlay, pack worker, script host, catalogue client, cover art and the GUI. Its second binary,
`ArmouryConsole`, is the console bar, compiled from the same sources the daemon's GUI uses for
fonts, DPI and net. `crates/armoury-core` holds the parts with no Windows in them, which is why
the whole engine can be tested without a desktop.

## Documentation

The published documentation is this README, **[deploy/README.md](deploy/README.md)** and
**[docs/modularity/README.md](docs/modularity/README.md)**. The pack format belongs to the packs
repository, linked from the Packs section above.

Everything else, including the changelog, handover notes, test checklists, decision log and
specifications, are working documents. They stay local and `.gitignore` keeps them out of the
repository the same way it keeps keys out.

## Licence

MIT. See [LICENSE](LICENSE).

## Credits and acknowledgements

**Built on these crates**, and grateful to their authors:

- [egui](https://github.com/emilk/egui) and [eframe](https://github.com/emilk/egui) for the GUI,
  with [winit](https://github.com/rust-windowing/winit),
  [glutin](https://github.com/rust-windowing/glutin) and
  [glow](https://github.com/grovesNL/glow) underneath.
- [Rhai](https://github.com/rhaiscript/rhai) for the pack script engine.
- [windows](https://github.com/microsoft/windows-rs) and
  [windows-sys](https://github.com/microsoft/windows-rs) for the Win32 surface.
- [ureq](https://github.com/algesten/ureq) with [rustls](https://github.com/rustls/rustls) and
  [rustls-platform-verifier](https://github.com/rustls/rustls-platform-verifier) for the
  catalogue and cover-art fetches, validated against the Windows certificate store.
- [minisign-verify](https://github.com/jedisct1/rust-minisign-verify) for the signed manifest,
  and [zip](https://github.com/zip-rs/zip2) for the helper archive.
- [serde](https://github.com/serde-rs/serde), [serde_json](https://github.com/serde-rs/json),
  [image](https://github.com/image-rs/image), [log](https://github.com/rust-lang/log),
  [crossbeam-channel](https://github.com/crossbeam-rs/crossbeam),
  [interprocess](https://github.com/kotauskas/interprocess),
  [tray-icon](https://github.com/tauri-apps/tray-icon),
  [sha2](https://github.com/RustCrypto/hashes) and
  [widestring](https://github.com/VoidStarKat/widestring-rs).

**The applications the packs drive are not part of this project.** G-Helper
([seerge/g-helper](https://github.com/seerge/g-helper)), Rainmeter
([rainmeter.net](https://www.rainmeter.net)) and Wallpaper Engine
([wallpaperengine.io](https://www.wallpaperengine.io)) are third-party software. The packs send
them their own documented commands (a hotkey chord, a bang over `WM_COPYDATA`, a CLI call) and
the daemon knows nothing about their internals.

**[SteamGridDB](https://www.steamgriddb.com)** provides the cover-art API the band falls back to.
The key is yours: you paste it into the console bar's window and it is stored in
`%APPDATA%\ArmouryDaemon\config.json` in plain text, because the daemon needs the raw value to
call the API. It is never shipped with the project.

G-Helper and ASUS's Armoury Crate already switch a laptop between profiles when a game starts.
ArmouryDaemon does the same for the whole desktop and leaves the switching to the tools that
already know how to do it.
