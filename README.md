# ArmouryDaemon

[![Licence: MIT](https://img.shields.io/badge/Licence-MIT-yellow.svg)](LICENSE)
[![Version](https://img.shields.io/badge/version-1.0.0-blue.svg)](Cargo.toml)

A resident Windows daemon that watches the applications you track and switches the rest of your
machine around them. While a tracked app runs, Rainmeter, G-Helper and Wallpaper Engine get out
of the way; when it closes, everything goes back to how it was.

Windows 11 · Rust · MIT

## What it is

ArmouryDaemon is a small always-on daemon — plus an on-demand GUI, in the same binary — that
reacts to the applications you care about. It runs **resident and elevated**: a scheduled task
starts it at logon and whenever Windows resumes from sleep, with the highest privileges, so
there is no UAC prompt afterwards.

You pick the apps you track. While one of them runs, the daemon applies **packs**. A pack is a
piece of data that says "while this app is running, tell *that* other program to do *this*, and
remember what it was doing so it can be put back". When the app closes, every pack reverts to
the state that was captured before it started.

Everything is local. There is no telemetry. The only network traffic is the optional pack
catalogue and the optional cover art.

## What it does

### Tracked applications

You choose the apps to track, either from the running windows or by browsing for an executable.
They are matched by **exe file name** (case-insensitively), so a game installed anywhere on disk
is the same tracked app. The daemon walks the process table once a second, diffs the PID set and
waits on the matching process handles, so an exit is exact and a PID reuse cannot confuse it.

### Packs

Three packs ship with the daemon, and more can be installed from the catalogue:

| Pack | What it does while a tracked app runs | When it closes |
|---|---|---|
| **Wallpaper Engine** | Stops, pauses or kills the animated wallpaper | Plays it again |
| **G-Helper** | Switches the performance mode (Turbo / Balanced / Silent) through G-Helper's own hotkeys | Restores the mode that was captured |
| **Rainmeter** | Loads another layout, or quits Rainmeter entirely | Reloads the `@Backup` snapshot |

A pack may expose up to **four dropdowns**; the first one is the primary action. The packs ship
with one each, except Wallpaper Engine, which has a second one for choosing between the 64-bit
and the 32-bit binary.

### The lockout ("Revert after")

Every change that actually happens starts a **lockout** — the settling window you see in the
Settings window as **Revert after**, 60 seconds by default. While a lockout runs, no other
change may touch the packs. An event that arrives inside that window is **deferred, not
dropped**: it lands when the lockout expires.

The lockout is a minimum spacing between two changes, not a scheduled switch. A lockout that
expires while the intent still holds changes nothing and starts no new one. A consequence worth
knowing: a **fast restart** (close the game and reopen it inside the lockout) never reverts at
all — the packs simply stay applied. Because the "previous state" is always captured on a
settled machine, the capture can never read the daemon's own value back.

Manual actions — removing a tracked app, pausing, entering or leaving idle mode — bypass the
lockout: they change the packs at once.

### The console band (the game launcher)

Plug in a controller and a dark, always-on-top tile strip opens along the bottom of the screen:
your tracked apps and any apps you pinned to it, with the cover art of each. The d-pad or the
left stick scrolls, **A** launches, **B** (or unplugging the pad) closes it. A mouse click
launches too, and `Esc` closes.

A **≡** button in the band's top-left corner opens a small menu for the artwork: **Choose
artwork…** opens a picker with an **Icon** and a **Background** tab — the 2:3 grids for the icon,
the wide heroes for the background — and **Use automatic again** hands the slot back to the
automatic picker. The mouse and a controller both work, a picture you pick is shown in the band
at once, and it stays until you take it away.

The daemon does the launching, so a game started from the band triggers the packs exactly as if
you had opened it by hand. A game that belongs to a Steam app is started the way Steam starts it
(`steam.exe -applaunch <appid>`); anything else is spawned directly. The daemon then checks that
the process really came up — a failed launch keeps the band open and says why instead of
sliding away.

The band can also be opened from the tray, and it can be told to stay out of the way: **Always**,
**On pad connect** (the default) or **Never**.

### Global hotkeys

| Key | What it does |
|---|---|
| **End** | Killswitch: hide the running tracked apps and mute their audio. Press again to put both back. |
| **Home** | Idle mode. Press again (or press any key) to come back. |

Both keys are handled by a **low-level keyboard hook** (`WH_KEYBOARD_LL`), not by
`RegisterHotKey`. That distinction matters: a registered hotkey is swallowed by Windows and
never reaches the foreground application, and `End` and `Home` are used in every text field,
every list, every IDE and every game. The hook calls `CallNextHookEx` and lets the key through,
so it still works everywhere else.

The killswitch keeps an exact ledger — the windows it hid and the audio sessions it muted, with
their previous state — and puts back only what it changed.

### Idle mode

**Home** blacks out every screen (one topmost window per monitor, with a grey *IDLE — press any
key to wake up* hint) and mutes every output device. Any **keypress** wakes it; the mouse
deliberately does not, so a nudged mouse cannot end an idle session. A failsafe timer force-quits
idle mode after a configurable number of minutes (180 by default).

The dashboard's idle band reports how many monitors it covered and how many outputs it muted.

While idle mode is up, pack switching is cancelled: tracked apps count as ordinary apps. The
packs are applied again when you wake it with a tracked app still running.

### Cover art

The console band shows the cover art of your apps. The automatic pick takes the **best** key art
it can find: the real 600×900 library picture for the tile, and for the background the widest
art that exists — five Steam assets are tried before **SteamGridDB** is asked at all, so a
missing file no longer means a **black** band. Without an API key the band falls back to the exe
icon and then to a monogram.

You are not stuck with the automatic pick. The **≡** menu in the band's top-left corner has
**Choose artwork…**, which opens a picker with an **Icon** and a **Background** tab — the 2:3
grids for the icon, the wide heroes for the background, each with its size and style under it.
The mouse and a controller both work, your choice is shown at once, and it keeps its place until
you choose **Use automatic again**.

### The pack catalogue

The catalogue has a window of its own: **Install packs** (Settings → **Install packs**). It
fetches the manifest when it opens and lists every pack in it. Every entry shows its version
and one of *Download* / *Update* / *vX installed*. The daemon downloads the file, checks its
**size and SHA-256** against the manifest and only then validates and installs it. Nothing
that cannot be verified is ever installed. The window's **Local pack** button opens the native
file picker and installs a `pack.json` from disk — the only way to install a local pack file.

From the command line:

```
ArmouryDaemon.exe --catalog-list
ArmouryDaemon.exe --catalog-install <id>
ArmouryDaemon.exe --catalog-url <url>
ArmouryDaemon.exe --version
ArmouryDaemon.exe --help
```

`--catalog-url <url>` lists that catalogue instead of the configured one. `--version` prints
`ArmouryDaemon 1.0.0`, `--help` prints the flags the binary accepts. An argument the binary
does not know is an error — the usage on stderr, exit code 2 — so the daemon never starts on
a typo. The release build is a windows-subsystem binary: from a terminal the line only shows
up when the output is redirected (`... --version 2>&1 | Out-String`), and it then goes to
stdout *and* stderr.

### The pack script host (spec 2 packs)

A pack may carry a small **Rhai** script in `script.apply` / `script.revert`. The pack never
executes it: the daemon compiles the source and runs it in a sandboxed interpreter that exposes
a whitelisted host API and nothing else — no `eval`, no imports, no ambient file or process
access. Every call is logged, the script gets a hard budget (500 000 operations, a 10 s wall
clock), and writes and program launches are restricted to `%APPDATA%`, `%LOCALAPPDATA%`,
`%PROGRAMDATA%` and the target app's own folder.

The generic host function is `ctx.window_message(class, title, payload)`: it finds a window of
another application by class and/or title and sends it the payload as a `WM_COPYDATA` message,
with a 2 s timeout. That is how the shipped Rainmeter pack loads a layout — the bang and the
window class are pack data, not daemon knowledge.

## What you see

There are **no screenshots in this repository**, so none are linked here. The daemon is a set of
small, undecorated windows plus a tray icon:

- **The tray icon** is a purple-magenta squircle with a bold white **A** (drawn procedurally, not
  a file). It turns into a red-slash variant while the killswitch is engaged. Left-click opens
  the dashboard, right-click opens the menu (open dashboard / pause all packs / idle mode /
  toggle killswitch / console mode / quit).
- **The dashboard** opens undecorated in the bottom-right corner, above the system tray. It shows
  the state, the daemon's version, the uptime, the cooldown, the pack count, the tracked apps
  (with a type-to-filter picker) and the packs, with a status band for *paused* and *cooling
  down*. Its Packs section also carries the built-in **Console launcher** row.
- **The Settings window** opens to the left of the dashboard: the **Revert after** value, the
  idle failsafe, the installed packs (each with a **Remove**), the console launcher's open mode
  and the apps pinned to it, the SteamGridDB key, and the two buttons **Install packs** and
  **Open packs folder**. The catalogue is checked in the Install packs window; the line under the
  packs row reports the **last check**, whoever ran it. The state, the daemon's version, the
  uptime, the cooldown, the pack count, the tracked apps and the catalogue list are **not** here
  — they are on the dashboard and in the Install packs window. The monitor count is no permanent
  row either: the dashboard's idle band reports it while idle mode is up. The window opens in a
  leftward chain: a new window goes immediately to the left of the current leftmost one, with
  a gap of exactly **5 physical pixels** (a literal pixel count, so the display's scale cannot
  change it; the window sizes still scale with the content and the scale).
- **The Install packs window** is the catalogue list: it opens to the left of the dashboard, and
  every pack in the catalogue gets a **Download**, an **Update** or a *vX installed* label at the
  right edge of its row, its description wrapping in a column of its own, plus a **Check for
  updates** button and the **Local pack** button.
- **One window per pack**, showing all of its dropdowns and its version.
- **The console band** is a wide, dark tile strip along the bottom of the screen.
- **The idle overlay** is a black, topmost window on every monitor.

## Install

The daemon is not on any package registry — it is installed from a folder you choose.

1. Copy `ArmouryDaemon.exe`, the `packs` folder and the `deploy` files (`setup.ps1`,
   `uninstall.ps1`, `README.md`) into a permanent folder, for example `C:\Apps\ArmouryDaemon`.
2. In an **administrator** PowerShell, in that folder:

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\setup.ps1
   ```

   `deploy\setup.ps1` registers a scheduled task that starts the daemon at logon and when Windows
   resumes from sleep, **with the highest privileges**, and starts it immediately. It writes the
   SHA-256 of the executable into an `ArmouryDaemon.exe.sha256` sidecar (and into the task's
   description), so a swapped binary is detectable, and it applies an explicit ACL to the folder:
   Administrators and SYSTEM keep full control, your account keeps *modify* (the daemon writes
   the `packs` folder there), everybody else gets read and execute only.

   The task is registered first on purpose: a failing ACL or a failing sidecar only warns and the
   script carries on, so the install can no longer leave the daemon down.

3. The tray icon appears. Left-click it for the dashboard.

**[deploy/README.md](deploy/README.md)** has the details: the exact ACL, updating (the running
executable is locked, so the order is stop → copy → run `setup.ps1`), and uninstalling.

## Using it

| What | Where |
|---|---|
| Dashboard | Left-click the tray icon, or **Open dashboard** in the tray menu |
| Settings | The gear in the dashboard's title bar |
| Tracked apps | The dashboard — the inline type-to-filter picker over all running processes, and **Browse…** |
| Packs | The **packs** folder in the install folder; Settings → **Open packs folder** |
| Config | `%APPDATA%\ArmouryDaemon\config.json` |
| Logs | `%APPDATA%\ArmouryDaemon\logs\daemon.log`, `ui.log`, `console.log` (rotated, 512 KiB per generation, three generations kept) |
| Cover art cache | `%APPDATA%\ArmouryDaemon\art` |
| Pack journal | `%APPDATA%\ArmouryDaemon\journal.json` |

The config is written by the daemon alone. A malformed config is never destroyed: its bytes are
kept next to it as `config.json.bad` and the daemon runs on the defaults. A config written by a
newer build is never overwritten by an older one.

## Building from source

You need the **MSVC Build Tools** (Visual Studio 2019 or 2022 — the daemon builds with either)
and a Rust toolchain (edition 2024, `rust-version` 1.95).

```powershell
cargo build --release        # -> target\release\ArmouryDaemon.exe
cargo test                   # the unit tests (205: 142 in armoury, 63 in armoury-core)
powershell -ExecutionPolicy Bypass -File tools\daemon-selftest.ps1   # the 72-check integration harness
```

**The release build is CPU-specific.** `.cargo/config.toml` adds `-C target-cpu=native` for
`x86_64-pc-windows-msvc`, so the executable is built for the machine that built it — this one is
an AMD Ryzen 7 4800H (Zen 2, AVX2-era). It will **not** run on a pre-AVX2 x86-64 machine, and a
new PC needs a rebuild. The release profile is `opt-level = 3`, `lto = "fat"`,
`codegen-units = 1`, `strip = true`, `incremental = false`.

The self-test harness needs no administrator rights and no Wallpaper Engine, G-Helper or
Rainmeter: it starts a throw-away daemon instance (its own pipe, config folder and single-instance
guard) and asserts the engine behaviour. It refuses to run against an installed copy.

## The packs

The packs live in their own repository: **[PixelAsh6/ArmouryDaemon_Packs](https://github.com/PixelAsh6/ArmouryDaemon_Packs)**.

A pack is a single `pack.json` file: which process it targets, the dropdowns it exposes, what it
captures before acting, and the ordered steps to apply and to revert. A pack is **data**, not
code — installing one can never run arbitrary code, and a spec 2 pack's script is interpreted by
the daemon against a whitelisted host API, never by the pack.

The catalogue is `catalog/index.json` plus one `packs/<id>/pack.json` per pack. The manifest
carries the version, the path, the SHA-256 and the size of every file; the daemon verifies both
before it installs anything.

## Project layout

```
Cargo.toml                  the workspace (two crates, one release profile)
crates/
  armoury-core/             config, protocol, status, engine, pack schema, logging, icon
  armoury/                  the single exe "ArmouryDaemon"
packs/                      wallpaper-engine, g-helper, rainmeter (one pack.json each)
deploy/                     setup.ps1, uninstall.ps1, README.md
tools/daemon-selftest.ps1   the 72-check headless integration harness
docs/                       research notes and plans
```

`crates/armoury` holds the daemon itself: the tray, the pipe server, the watcher, the hotkey
hook, the killswitch, the idle overlay, the cover-art fetcher, the console launcher, the pack
worker, the script host and the catalogue client. `crates/armoury-core` holds the parts with no
Windows in them: the trigger engine, the config, the pack schema and the wire protocol.

## Documentation

| File | What is in it |
|---|---|
| [HANDOVER.md](HANDOVER.md) | The state of the project: what is built and verified, the known issues, the gotchas |
| [testing.md](testing.md) | The manual test checklist — the open items only |
| [DECISIONS-2026-10-17.md](DECISIONS-2026-10-17.md) | The decision log: every behavioural decision and why it was made |
| [SPEC3-SCRIPTING.md](SPEC3-SCRIPTING.md) | The pack script host: the format, the API, the limits and the examples |
| [deploy/README.md](deploy/README.md) | Install, update, uninstall, and how to write a pack |
| [PACK-SPEC-PROPOSAL.md](PACK-SPEC-PROPOSAL.md) | The original data-only pack model (the script mechanism supersedes it) |

## Licence

MIT — see [LICENSE](LICENSE).

## Credits and acknowledgements

**Built on these crates**, and grateful to their authors:

- [egui](https://github.com/emilk/egui) and [eframe](https://github.com/emilk/egui) — the GUI.
  With [winit](https://github.com/rust-windowing/winit),
  [glutin](https://github.com/rust-windowing/glutin) and
  [glow](https://github.com/grovesNL/glow) underneath it.
- [Rhai](https://github.com/rhaiscript/rhai) — the pack script engine.
- [windows](https://github.com/microsoft/windows-rs) and
  [windows-sys](https://github.com/microsoft/windows-rs) — the Win32 surface.
- [ureq](https://github.com/algesten/ureq) with
  [rustls](https://github.com/rustls/rustls) and
  [webpki-roots](https://github.com/rustls/webpki-roots) — the catalogue fetch.
- [serde](https://github.com/serde-rs/serde),
  [serde_json](https://github.com/serde-rs/json),
  [image](https://github.com/image-rs/image),
  [log](https://github.com/rust-lang/log),
  [crossbeam-channel](https://github.com/crossbeam-rs/crossbeam),
  [interprocess](https://github.com/kotauskas/interprocess),
  [tray-icon](https://github.com/tauri-apps/tray-icon),
  [sha2](https://github.com/RustCrypto/hashes) and
  [widestring](https://github.com/VoidStarKat/widestring-rs).

**The applications the packs drive are not part of this project.** G-Helper
([seerge/g-helper](https://github.com/seerge/g-helper)), Rainmeter
([rainmeter.net](https://www.rainmeter.net)) and Wallpaper Engine
([wallpaperengine.io](https://www.wallpaperengine.io)) are third-party software. The packs only
send them their own documented commands — a hotkey chord, a bang over `WM_COPYDATA`, a CLI call —
and the daemon knows nothing about their internals.

**[SteamGridDB](https://www.steamgriddb.com)** provides the cover-art API used by the console
band. The API key is your own: you paste it into Settings and it is stored in
`%APPDATA%\ArmouryDaemon\config.json` — in plain text, because the daemon needs the raw value to
call the API. It is never shipped with the project.

The idea is not new. G-Helper and ASUS's Armoury Crate already switch a laptop between profiles
when a game starts. ArmouryDaemon does the same for the whole desktop and leaves the switching
itself to the tools that already know how to do it.
