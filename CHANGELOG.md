# Changelog

All notable changes to ArmouryDaemon are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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

[1.1.1]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.1
[1.1.0]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.0
[1.0.0]: https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.0.0
