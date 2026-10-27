# Changelog

All notable changes to ArmouryDaemon are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.3.0] - 2026-10-27

### Added

- **The console launcher is its own executable.** `ArmouryConsole.exe`
  (`--console`'s band, built from the same `console_ui`/`art`/`pad` sources
  the daemon binary compiles) is the "helper module" shape of Phase C: a
  catalogue entry of kind "helper" whose archive carries
  `ArmouryConsole.exe`.
- **The daemon prefers the installed console helper.** `show_console`
  spawns the first `modules/<id>/ArmouryConsole.exe` a helper module
  recorded (`module.json`'s `entry`, matched by file name, lower id first,
  a module whose exe is gone is skipped), logs its id, version and path,
  and falls back to `current_exe() --console` - the old path, unchanged in
  behaviour - when no helper is installed
  (`docs/modularity/DECISIONS.md` §29).

### Changed

- The process-wide HTTP client moved from `catalog.rs` to a shared
  `net.rs` (`net::agent`), so the console exe needs no catalogue machinery;
  `catalog::agent` retired. The system font install (`ui.rs`) and the
  Per-Monitor-V2 awareness switch (`main.rs`) moved to `fonts.rs` and
  `dpi.rs` the same way. No behaviour changed: same client, same fonts,
  same DPI switch, one more binary.

### Fixed

- The catalogue check's error line wraps too. When a check fails before any
  row is painted - a signature fetch or verification failure, or no network
  at all - the message is painted in the rows' place, and that line could
  run wide enough that the window edge cut it mid-URL. It wraps like the row
  failure line now: the second arm of the same fix.

## [1.2.0] - 2026-10-27

### Added

- The **catalogue manifest arrives minisign-signed**. `catalog::fetch` gets
  `index.json` and `index.json.minisig`, verifies the signature against a
  keyring baked into the binary **before** parsing anything, and refuses an
  unsigned or badly signed manifest by name, nothing installed or listed
  (`docs/modularity/DECISIONS.md` §26).
- The **catalogue manifest reaches schema 2**. The entries move under
  `modules` and each names its `kind` — `"pack"` or `"mode"`, the same
  vocabulary a pack file uses; an unknown kind refuses the manifest with the
  entry's name, an absent kind means `pack`, and the build reads both keys
  for one release so an old manifest keeps working (a manifest carrying both
  is refused, `docs/modularity/DECISIONS.md` §25).
- **Helper modules install like packs, and lock their folder.** A catalogue
  entry of kind "helper" names an archive plus the executable inside it
  (`entry`); `--catalog-install` verifies the archive's SHA-256 and size
  against the manifest, extracts it into `modules/<id>/` - the folder and its
  parent carry an Administrators + SYSTEM full, user read + execute lock -
  records the layout in the folder's `module.json`, and a later
  `--module-list` reads every installed pack, mode and helper back from
  disk. A helper never joins the pack or mode worker
  (`docs/modularity/DECISIONS.md` §27).
- The **helper Download button is armed**. The Modules window downloads the
  helper archive on its own worker thread and verifies it against the
  signed manifest, then hands the daemon the new `InstallHelper` request -
  the entry's id, version, executable path, SHA-256 and size. The daemon
  re-verifies the bytes after it reads them, extracts into `modules/<id>/`
  behind its folder locks, removes the temp archive whatever the answer is,
  and answers through the usual pack-op reply
  (`docs/modularity/DECISIONS.md` §28).
- The **catalogue window is the "Modules" window**. All three kinds render
  in one list - packs and modes keyed off the daemon's pack status,
  helpers off their own `module.json` record on disk, with the archive's
  size on the helper's (disabled) button: helpers install through the
  daemon from the next phase. An installed pack's row carries a
  permission line under its description - the granted capabilities as
  effects, in one cached line (`docs/modularity/DECISIONS.md` §28).
- The **killswitch has a config key and an IPC request**. One field,
  `killswitch_enabled` (default on), is both the switch and the "is the killswitch
  module installed" flag: while it is off, the End key and the tray toggle are
  logged no-ops and the tray item reads "Killswitch off in config";
  `{"type":"set_killswitch"}` carries the decision over the pipe; the state
  survives a restart; turning the switch off releases an engaged killswitch first
  (windows back, audio back, tray icon back); and a status from an older daemon
  still means on (`docs/modularity/DECISIONS.md` §23).

### Fixed

- **Quitting releases an engaged killswitch.** The quit path reverted the packs and
  left idle mode, but an engaged killswitch rode past both: its hidden windows die
  with the process, while its mutes live on the audio devices — the machine could
  stay hidden and silent until the daemon started again. The killswitch is now
  released between the revert and the exit.
- **A failed download leaves the row offering its button.** The Modules window's
  last-op line painted with the window's extend-wide mode and no width cap, so a
  wide fetch error widened the list and laid the rows' right-aligned buttons past
  the window edge, where they clipped away; the button came back only when the line
  was cleared - a settings reopening or a restart. The line wraps like the notes
  under the rows now, proven by the paint test
  `a_failed_download_leaves_the_row_offering_the_button`.

### Changed

- The **exe carries its own C runtime**. The MSVC build now links the CRT
  statically (`-C target-feature=+crt-static`), so it imports no
  `VCRUNTIME140.dll`: the lone exe starts even on a stripped image where the VC
  redistributable was never installed, at the cost of +188 KiB measured on the
  CI artefact (`docs/modularity/DECISIONS.md` §24).

- The **pack worker is no longer one serial thread**. The plain packs keep their thread; the
  mode packs get a **second worker and channel of their own**, and the daemon splits a
  whole-state apply or revert between the two by the pack's `kind` and merges the halves back
  on one outcome channel, so the running state and the journal read as they always did. A mode
  whose steps take five seconds no longer delays the apply or the revert of a pack beside it:
  the self-test harness measures the two against each other, from log timestamps, in both
  directions. Nothing changes for a pack - `kind` defaults to `"pack"`, and a mode still
  arrives only through the ordinary tracked-app trigger (`docs/modularity/DECISIONS.md` §22).

## [1.1.6] - 2026-10-22

### Added

- The pack schema carries **`kind`** and **`triggers`**. A pack may say what it is
  (`"kind": "pack"` or `"mode"`) and what starts it (`"triggers"`). Both fields are
  **additive**: a pack that does not carry them loads and behaves exactly as it did, the
  three shipped packs carry neither, and the catalogue needs no change. A kind outside the
  set is refused by name - `unknown kind 'widget' (expected 'pack' or 'mode')` - and never
  a silent default to `"pack"`;
  a mode needs at least one trigger, a `hotkey` trigger needs a key, and a `tracked_app`
  trigger takes none. `spec` stays 1 or 2.
- A **`registry`** capability, for the `registry_set` step. A pack that writes a registry
  value has to ask for it (`"capabilities": ["registry"]`); a pack that declares no
  capabilities still gets every one of them.

### Fixed

- The **capability gate did not cover the declarative steps**. A pack that declared
  `"capabilities": ["files"]` - or `[]` - could still run programs (`cli`), inject
  keystrokes (`hotkey`), kill processes (`process_kill`) and write the registry
  (`registry_set`): the script was gated, the steps were not. Every mechanism now names a
  capability and a step the pack did not ask for is refused **when the pack is parsed**, so
  it never loads - and refused again where the step would run.
- A change of the cooldown **brought a window that had ended back to life**. The daemon
  drains a pipe request before the 1 Hz tick that reaps an expired window, so a
  `set_cooldown` in that gap re-timed a dead window: the dashboard showed a countdown and
  the next game launch was held for a cooldown that was never really running. Only a window
  that is still running is re-timed now; one that has ended is dropped.
- **`overlay_show`/`overlay_hide` requests queued without a limit.** Every request cost a
  black window per monitor, a write and a delete of `idle.marker` and a pack job, so a
  script with a thousand pairs kept the daemon thrashing for minutes. There is one pending
  request now, and a request for the state the daemon is already in is skipped.
- The **denial message was lost inside a script-defined function**. Rhai wraps an error
  raised in a script function, so the daemon's match missed it and the pack's log got
  Rhai's raw `Function not found` instead of `run_cli REFUSED: it needs the 'exec'
  capability…`.
- **`audio_mute_all` left no crash-recovery record.** Its devices lived in an in-memory
  list only, so a crash or a reboot left the machine muted. They go into `idle.ledger` now,
  the file idle mode already uses, and the next start puts them back.
- A **hand-edited `cooldown_seconds` could panic the daemon** at startup: the value became
  an `Instant + Duration`, and nothing clamped it. It is clamped to 5-3600 s, the range the
  UI writes.

## [1.1.5] - 2026-10-22

### Added

- The pack script's host API is **scoped by capability**. A pack declares the capabilities it
  wants (`"capabilities": ["files"]`) and the daemon grants only those. The sandbox keeps **one
  engine per capability set** and registers only the granted functions on it, so a call to a
  function of a capability that was not granted fails as an **unknown function** — the denial is
  structural, not a check a call site could forget. The denial names the missing capability and
  what the pack asked for, and it lands in the pack's log. The field is **additive** and defaults
  to every capability, so the shipped packs keep working unchanged.
- The host functions report **what they changed**: `audio_mute` names every session of the
  process and the state it was in before the call, and it never unmutes a session the user had
  muted themselves.

### Fixed

- The catalogue fetch validated TLS against **Mozilla's bundled roots** instead of the **Windows
  certificate store**, so it failed on any machine with a corporate CA or with TLS interception.
  The root store is the platform's now: the enterprise roots, the constraints and the revocation
  checks of the machine's own certificate store decide what is trusted.
- The **cover art** was fetched with the client's default configuration — Mozilla's bundled roots
  — so the same bug was left in a second place: a machine behind a corporate CA could fetch the
  catalogue but not the art. The art fetches go through the **same client** as the catalogue now
  — one agent, built once, with the platform verifier — so they share its root store and its
  connection pool, and no client is built for a request any more. The Mozilla bundle leaves the
  binary with it: the build has exactly one root store, and the client refuses the bundled roots
  instead of falling back to them.
- A change of the cooldown did **not** re-time the window that was running: it changed only the
  length of *future* windows, so the window in flight ended at the old expiry and the change it
  was holding landed in that gap — the packs went back to their idle state while the game was
  starting, and the game's return then sat held inside the new window with the packs off. The
  window keeps its **start** and takes the **new length** now, so it ends at `start + cooldown`,
  and the change it holds lands at the new expiry.
- The dashboard said only "Cooling down — 2s" and gave no hint which app the window belonged to,
  and the cooldown row said nothing about a change waiting behind the window. The band names the
  app now, and the row says that a change is waiting — the daemon publishes what the window is
  holding, so the queue is visible.

### Changed

- The catalogue fetch reads the **WinINet proxy settings** of the user. This is a per-user
  desktop app, so the proxy of the desktop session is the right one — WinHTTP would be for a
  Windows *service*.
- The catalogue responses are **compressed**: the client asks for `gzip` and decodes it. The
  bytes that are verified against the manifest are the plain ones, as before.

## [1.1.4] - 2026-10-22

### Fixed

- The lockout held a change in one direction only. A tracked app that started while a window was
  running got the packs at once, so the machine changed in the middle of a cooldown, and a close
  that landed between two windows put the machine back at once — the restart that followed then
  waited out the revert's own window, so the machine sat in the quiet mode while the game ran and
  the logs showed the revert before the apply. The lockout now holds a change in **both**
  directions: a **close is always held** (it starts a window if none is running and keeps the
  running one rather than restarting it), a **start inside a window is held** and lands at the
  expiry — if the app is still running by then — **exactly once**, a **restart inside the window
  cancels the hold**, and a **manual action still bypasses** it. A start with no window running
  still applies at once: that is the first launch of a session.

### Changed

- The release carries **only the executable and its hash sidecar** now. There is no zip and no
  scripts in it: the executable installs itself, and the scripted install lives in the
  repository's `deploy\` folder.
- The CI and release workflows pin the toolchain to **1.98.0** instead of `stable`, so two runs of
  the same commit are built by the same compiler. The CI workflow can also be run by hand, and it
  then hands over the release build as an artefact, so a binary can be tested before a release
  exists.

## [1.1.3] - 2026-10-21

### Fixed

- The lockout held the wrong change. A tracked app that started had to wait for the lockout
  before the packs came on, and a close that landed between two lockouts put the machine back
  at once — so the packs flipped back and forth under the running game. The apply is not
  deferred any more: a tracked app that starts gets the packs at once. The lockout holds the
  **revert** now: a close waits for the window, a fast restart inside it cancels the wait, and
  the revert lands when the window is over — if the app is not running by then.
- A tracked app that exited and was replaced by a new one on the same PID was reported as
  stopped and never as started, so the packs went back and never came back. A recycled PID is
  a stop followed by a start now.
- The pack script's sandbox built a new interpreter and registered its host functions on every
  run. It keeps one engine now: the 432.7 µs the build used to cost is paid once, and a run
  spends 4.7 µs on the engine.
- The killswitch unmuted an audio session the user had muted themselves. Only the sessions it
  changed are restored now.

### Changed

- The Settings window calls the lockout **Cooldown**. The setting itself is unchanged: the same
  lockout window, the same seconds and the same `cooldown_seconds` key in `config.json`. Only the
  label changed, so it matches the dashboard's "cooldown" and "Cooling down".
- The build is no longer specific to the machine that compiled it. The build now targets the
  fixed **x86-64 v3** level — AVX2, BMI1, BMI2, F16C, FMA, LZCNT, MOVBE and XSAVE — so the
  executable is the same on every machine, and it runs on Intel Haswell (2013) and later and AMD
  Excavator (2015) and later without being rebuilt for another PC.
- The killswitch enumerated the machine's audio sessions once per tracked app. It enumerates
  them once for the whole press now, and restores from its ledger on release without enumerating
  at all: about 45 ms off the event loop on each press.
- The daemon walked the process table every tick, even with nothing tracked. The walk is skipped
  when no applications are tracked: it cost about 19.5 ms a second, and it now happens only when
  there is something to find.
- The console band is centred on the screen again and is allowed to cover the dashboard, because
  the band is used to launch a game from a controller while the user is away from the computer.
- The band's corner radius and bottom fade are point values, and its corner arc is sampled
  according to the physical pixel size, so the drawing scales with the display.

## [1.1.2] - 2026-10-21

### Added

- The packs are listed alphabetically by name in the dashboard, in the Settings window and in the
  Install packs window. The built-in console launcher stays where it is, and the order the packs
  are **applied** in is unchanged.

### Changed

- A JSON error in a pack file now names the field that is wrong, instead of a line and a column.

### Fixed

- The Install packs window opened only after its six-second settle deadline, because its size was
  compared in logical points while Windows rounds the window to whole physical pixels. It now opens
  in about 50 ms, and the same fault in the Settings window and the pack windows is fixed.
- Asking for a second pack while the first was downloading dropped the first one silently. The
  in-flight state is now tracked per pack.
- A pack's temporary file name was built from its id, which comes from the catalogue manifest, so a
  crafted id could write outside the temporary folder. The name is sanitised now.
- The pack validator's duplicate-setting check was quadratic, so a pack file with 200,000 dropdowns
  took over ten minutes to validate; the check is now linear, and such a pack is refused in seconds.
- The daemon's pipe read a message into an ever-growing buffer, so a client that never sent a
  newline could make the daemon allocate without limit; a message is now capped at 1 MiB and
  anything longer is refused like a malformed one.
- The daemon looked for its `packs\` folder in the executable's folder and then in that folder's
  grandparent, so a lone executable could pick up a `packs\` folder from the wrong place; the
  grandparent is only searched when it is a source checkout.
- The file the catalogue downloads is now deleted once the daemon has installed it, whether the
  install succeeded or failed; a pack file the user picked in the file dialog is never deleted.
- The dashboard's empty state said "No packs found (looked in `<app>`/packs)", with an
  unsubstituted placeholder; it now says no packs are installed and points at Settings → Install
  packs.
- The dashboard opened on the first run only when no packs were installed, so an install that
  shipped the three packs never showed it; it now opens on the first run whatever is installed.
- The console band was laid out across the whole width of the screen and covered 58.8% of the
  dashboard, so a click meant for the dashboard landed on a band tile and launched that
  application. The band now leaves the dashboard's column clear and the two no longer overlap.

## [1.1.1] - 2026-10-21

### Fixed

- The Install packs window's **Local pack** button was pushed out of the bottom of the window by
  dead space above the footer, so it could not be clicked. The catalogue list now shrinks to the
  height its rows need, the footer always fits, and a long catalogue still scrolls.
- The tracked-app picker now says how many matches it did not show, instead of silently stopping
  at eight.

## [1.1.0] - 2026-10-20

### Added

- The executable is now a complete installation on its own. It carries a manifest that asks for
  administrator, creates the `packs\` folder next to itself when it is missing, registers its own
  scheduled task (at logon and on resume from sleep, with the highest privileges) and opens the
  dashboard on the first run. The pack catalogue needs no API key; the SteamGridDB key is for the
  cover art only.
- When the daemon registers its own task it also writes the executable's SHA-256 sidecar and
  records that hash in the task's description, byte-for-byte as `setup.ps1` does, and warns when a
  task's recorded hash differs from the running binary.
- The executable carries the daemon's own icon as a PE resource, so Explorer, the desktop, the
  shortcuts and the pinned taskbar item show the purple squircle instead of the default Windows
  application icon.

### Fixed

- Nine defects found by a code review: leaving idle mode while the daemon is paused no longer
  leaves the packs reverted while the dashboard claims they are applied; a pack step that does
  nothing is no longer reported as a success; a non-zero command-line exit is information unless
  the step asks for it; the icon fallback no longer destroys a handle the shell owns; tracked apps
  are matched by name when they are removed; the catalogue URL is read once; the killswitch count
  reports what it says; the retired dashboard position is gone; the dead code is deleted.
- A refused key injection names the Windows error behind it and is retried once; the command-line
  wait blocks on the process handle instead of polling; the dashboard reads the running-app list
  from a worker thread instead of walking the process table on the drawing thread; the console
  band no longer opens over a game that is already running.

## [1.0.0] - 2026-10-20

### Added

- The first release: a resident daemon that watches the applications you track and applies packs
  while they run, and puts everything back the way it was when they close.
- Three shipped packs — **Wallpaper Engine 1.4.0**, **G-Helper 1.3.1** and **Rainmeter 1.5.0** —
  and the pack catalogue they are published in.
- The dashboard, the Settings window and the Install packs window.
- The console band, the game launcher, with its artwork picker.
- The killswitch and idle mode, on the **End** and **Home** keys.
- The reversible pack journal — `%APPDATA%\ArmouryDaemon\journal.json` — which records what each
  pack changed, so a revert puts the machine back the way it was.
- The spec 2 pack script host: a pack may carry a small Rhai script, and the daemon runs it in a
  sandboxed interpreter.

[1.2.0]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.2.0
[1.1.6]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.6
[1.1.5]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.5
[1.1.4]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.4
[1.1.3]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.3
[1.1.2]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.2
[1.1.1]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.1
[1.1.0]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.0
[1.0.0]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.0.0
