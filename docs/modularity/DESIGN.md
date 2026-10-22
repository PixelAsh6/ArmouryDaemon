# Design

The concrete shapes for the module system. `DECISIONS.md` says *why*; this says *what*.

---

## 1. The pack schema (additive — every shipped pack keeps working)

```jsonc
{
  "spec": 2,                              // 1 = data, 2 = data + script. UNCHANGED.
  "id": "killswitch",
  "kind": "mode",                         // "pack" (default) | "mode"
  "triggers": [ { "kind": "hotkey", "key": "end" } ],
  "suspends_packs": false,               // idle sets true
  "react_to_tracked": true,               // killswitch sets true
  "capabilities": ["windows", "audio"],
  "apply":  { "steps": [] },
  "revert": { "steps": [] }
}
```

| Field | Default | Meaning |
|---|---|---|
| `kind` | `"pack"` | `"pack"` applies when a tracked app runs; `"mode"` is a global on/off thing |
| `triggers` | `[{ "kind": "tracked_app" }]` | what starts it. `"hotkey"` with a key name is the first new kind |
| `suspends_packs` | `false` | while on, freeze the tracked-app engine (`Engine::set_idle`) |
| `react_to_tracked` | `false` | a tracked app that starts while this is engaged is handled too |
| `capabilities` | **every capability** | what this pack asks the host for (§2) |

**The default of `capabilities` is every capability, not none.** The three shipped packs
predate the field, and the Rainmeter pack's script uses `window_message` — a pack that
asked for nothing would lose the host functions it uses. The user's own grant list (the
permission window) comes later and narrows this down; the field is what the pack
*asks* for, and until the user can narrow it, the ask is everything.

**Do not invent a `spec: 3`.** `spec` is the *script* axis and stays 1 or 2. `kind` and
`triggers` are new, separate axes.

**Triggers that must work:** `tracked_app` (the default, today's behaviour) and
`hotkey` (a key name that maps to the daemon's existing hotkey table —
`crates/armoury/src/hotkey.rs` already knows End and Home).

---

## 2. The capability taxonomy

The 25 Rhai host functions (`crates/armoury/src/script.rs`, the registration block) group
into capabilities. A pack declares what it needs; the daemon grants only that. The last
three groups are **new** and are exactly what the killswitch and idle need.

**The table as it was written down was missing one function.** It listed 17 of the 18
existing host functions: `sleep` (`script.rs:308` before this phase) was in **no**
capability, and under per-capability-set engines a pack whose script called it would have
failed as an unknown function. `sleep` is now a capability of its own — see
`DECISIONS.md` §14 for the reason and for what was rejected.

| Capability | Functions | Severity | Notes |
|---|---|---|---|
| `context` | `target_name`, `target_exe`, `target_dir`, `setting`, `captured` | none | read-only |
| `journal` | `capture`, `log` | none | |
| `files` | `read_file`, `read_json`, `read_ini` | low | reads anywhere |
| `files-write` | `write_file` | low | already sandboxed to four roots |
| `process` | `process_running`, `process_kill`, `process_start` | medium | start/kill restricted to the allowed folders |
| `exec` | `run_cli` | medium | restricted to the allowed folders |
| `input` | `hotkey` | **high** | injects keystrokes |
| `ipc` | `window_message` | **high** | sends `WM_COPYDATA` to any window |
| `windows` *(new)* | `window_hide`, `window_show`, `windows_of` | **high** | hides other apps' windows |
| `audio` *(new)* | `audio_mute`, `audio_mute_all`, `audio_sessions` | **high** | mutes any process or device |
| `overlay` *(new)* | `overlay_show`, `overlay_hide` | **high** | full-screen takeover |
| `time` *(new)* | `sleep` | none | a flow-control primitive: it grants nothing |

The new host functions are thin wrappers over code that already exists and must stay
where it is:

| New function | Wraps | Notes |
|---|---|---|
| `windows_of(pid)` | `killswitch::windows_of` — `killswitch.rs:202` | the visible, unowned, non-tool windows of a process |
| `window_hide(hwnd)` / `window_show(hwnd)` | `ShowWindow`, as in `killswitch.rs:137` | must report the *previous* visibility so a revert is exact |
| `audio_mute(pid, on)` | the session sweep — `killswitch.rs:236`, `killswitch.rs:323` | must return the previous mute state, and must not unmute a session the user muted |
| `audio_mute_all(on)` | `overlay::mute_all` — `overlay.rs:499` | for idle mode |
| `overlay_show()` / `overlay_hide()` | `overlay::show` / `overlay::hide` — `overlay.rs:248`, `overlay.rs:347` | the daemon keeps owning the crash-recovery ledger and the keypress dismissal |

**`overlay_show` is a request, not a call.** The black windows belong to the thread that
pumps their messages, and that is the daemon's main loop — the pack worker cannot show
them itself. The host function therefore hands the request over (`packs.rs:1253`), and
the daemon's main loop carries it out (`daemon.rs`), which keeps owning the
crash-recovery ledger, the keypress dismissal and the idle-mode policy.

**The killswitch and idle modes are then almost trivial packs**, because the ledger
and the crash recovery stay in Rust:

```jsonc
// a killswitch pack
"apply":  { "steps": [] },
"revert": { "steps": [] }
```
with the host function `killswitch_engage(tracked_pids)` / `killswitch_release()` added
as a **single** capability verb rather than the three separate ones — it is the
daemon's own ledger, and splitting it would let a pack half-apply it. Decide this
while implementing; the taxonomy above is the shape, not a fixed list.

---

## 3. How the capability gate is enforced

**One engine per capability set.** The sandbox keeps **one engine per thread** since
1.1.3 (`script.rs:90`, `ENGINES`), and the host API is registered on it once. So
"register fewer functions per run" is no longer free.

A second thread-local map, built lazily, capped like the AST cache (32 entries):

```
thread_local! {
    static ENGINES: RefCell<HashMap<CapSet, Sandbox>> = RefCell::new(HashMap::new());
}
```

The pack worker is a **single serial thread** (`packs.rs:160`), so no locking is
needed. A pack then gets an engine on which **only the granted functions are
registered** (`script.rs:298`, `new_sandbox`), so an ungranted call fails as an
*unknown function* — a structural denial, not a runtime check a call site could forget.
Each distinct capability set costs ~433 µs to build, once.

The key of the map is the **capability set** (`armoury_core::pack::CapSet`), a bitmask
over the twelve capabilities: two packs that ask for the same capabilities share one
engine, and a pack that asks for nothing gets an engine with no host function on it at
all.

The per-run budget re-arming (the `Sandbox::arm` pattern) still applies per engine:
the operation budget and the wall clock must be re-armed before **every** evaluation,
so a run can never inherit the previous run's allowance — not even the run before it on
another engine.

**Two alternatives, if the first does not work out:**
1. One shared engine plus a dispatcher — register one function per capability that
   consults the running pack's grant set at call time. Rhai's `NativeCallContext`
   exposes the scope, so the pack id can be put in the `Scope` and read back. Cheaper
   in memory, but the denial is a runtime branch.
2. One engine per pack — **throws away the 1.1.3 caching win. Do not.**

Rhai has no `unregister_fn`, and `disable_symbol` only disables keywords and
operators, so "register less" and "check at call time" are the only two mechanisms.

**The error the user sees** must name the missing capability and the one the pack
asked for, and the denial must be written to the pack's log like every other host
call. A denied call comes back from Rhai as `ErrorFunctionNotFound`; the daemon reads
the function name out of it (`script.rs:265`, `denial`) and turns it into:

```
run_cli REFUSED: it needs the 'exec' capability, which this pack was not granted (the pack asked for: files)
```

That line is what the pack's log gets, and it is the whole error the daemon records.
A function the pack **was** granted but called wrongly (a wrong argument type) comes
back the same way from Rhai — that is not a denial and is not reported as one.

---

## 4. The catalog manifest

```jsonc
{
  "schema": 2,
  "updated": "2026-10-22",
  "modules": [
    { "id": "g-helper", "kind": "pack",   "name": "G-Helper", "version": "1.3.1",
      "file": "packs/g-helper/pack.json", "sha256": "…", "size": 1235, "min_engine": "1.0.0" },
    { "id": "killswitch","kind": "mode",  "name": "Killswitch", "version": "1.0.0",
      "file": "packs/killswitch/pack.json", "sha256": "…", "size": 900 },
    { "id": "console",   "kind": "helper", "name": "Console launcher", "version": "1.0.0",
      "file": "modules/console/console.zip", "sha256": "…", "size": 2400000,
      "entry": "ArmouryConsole.exe" }
  ],
  "signature": { "algorithm": "ed25519", "key": "…", "value": "…" }
}
```

- `packs` becomes `modules`; **read both for one release** so an old manifest still
  works. `kind` defaults to `"pack"` so an old entry needs no change.
- `entry` is the executable inside the archive, for `helper` kinds.
- The manifest's `file` path is **repository-relative**, not manifest-relative. This
  caused a real 404 in testing — do not "fix" it by making it relative to the
  manifest.
- The release workflow's "Check the packs against the catalogue" step
  (`.github/workflows/release.yml`) reads `$catalog.packs`; **it must learn the new
  key** and must keep failing when the release ships bytes the catalogue does not
  publish.

---

## 5. Trust

- **minisign / Ed25519** via `minisign-verify` 0.3.0, over the manifest bytes.
- The trust anchor is a **keyring baked into the exe**, in a file such as
  `crates/armoury/src/trust/keys.json`, read with `include_str!`. Two keys live at a
  time; rotation = release a build with the new key added and the old one still
  valid (90 days), then a later build that drops the old one. A manifest is accepted
  if any non-expired anchor signed it.
- Verify the signature **before** parsing the manifest. Refuse the whole manifest on
  a bad signature. Never install anything unverified.
- The payload's SHA-256 and size are checked after download, exactly as today.
- **Never** an HKCU trust anchor, and **never** `rustls::dangerous` in any build.

---

## 6. The GUI

- Rename "Install packs" → **"Modules"**. The window is
  `crates/armoury/src/ui.rs:2339` (`install_packs_ui`); the row renderer is
  `ui.rs:2595` (`catalog_row`).
- One list, three row shapes:
  - `pack` / `mode` → *Download* / *Update* / *vX installed*
  - `helper` → *Download (2.3 MB)* / *Update* / *vX installed*
- The dashboard's built-in "Console launcher" row becomes a normal module row. It is
  `ui.rs:61` (`CONSOLE_FEATURE`), used at `:987`, `:2042`, `:2230`, `:4486`, `:4501`.
- Per module, a flat human-readable capability list, revocable — the Chrome / Firefox
  permission-UI shape. **Name the effect, not the API.** Something like:

  ```
  Killswitch (by @publisher)
    ☐ Hide windows belonging to other apps
    ☐ Mute the audio of other apps
    ☐ Read/write its own config file
    ☐ Run external programs        ← separate, explicitly labelled group
  ```
- **Discovered dropdown values.** The last piece of per-pack hardcoding is
  `crates/armoury/src/daemon.rs:733` (`if p.pack.id == "rainmeter"`) plus
  `packs::rainmeter_layouts()` (`packs.rs:795-814`). A pack must be able to declare
  that its dropdown values are *discovered* (e.g. from a directory) rather than
  hardcoded in the daemon.

---

## 7. Config migration

`AppConfig.packs: HashMap<String, PackUserConfig>` (`crates/armoury-core/src/config.rs:51`)
is already the right shape — `PackUserConfig { enabled, values }` is exactly what a
module needs.

- Rename `packs` → `modules`.
- Add `"packs"` to `RETIRED_KEYS` (`config.rs:42`) so it is dropped on read.
- Migrate the values on load; never lose an install.

---

## 8. The daemon's side

| Concern | Where | What changes |
|---|---|---|
| Mode worker | `packs.rs:149` (`packs::start`) | a **second** worker thread and channel for modes, so a slow mode never blocks a pack apply/revert |
| Mode triggers | `daemon.rs:1693` / `:1697` (the hotkey handlers) and the tray menu | a mode's trigger dispatches to its own worker |
| `suspends_packs` | `daemon.rs:1710` (`enter_idle`) / `:1743` (`exit_idle`) | the mode drives `Engine::set_idle` |
| `react_to_tracked` | `daemon.rs:1334-1339` | a tracked app starting while the mode is engaged re-engages it |
| Installing a module | `daemon.rs:2556` (the `InstallPack` handler) | one handler per kind |
| The console helper | `daemon.rs:2008` (`show_console`) | spawn the installed helper, fall back to `current_exe() --console` |
| The killswitch | `daemon.rs:1778` (`toggle_killswitch`) | it has **no config key and no IPC request** today; add both |

The pack worker is a **single serial thread** (`packs.rs:160`,
`while let Ok(job) = jobs.recv()`) — that is the reason for the second worker.

---

## 9. The helper module (console)

- A new `[[bin]]` in `crates/armoury/Cargo.toml` (one already exists at `:7-9`).
  `console_ui.rs` (2680 lines) + `art.rs` (1395 lines) + `pad.rs` become
  `ArmouryConsole.exe`; the daemon keeps the dashboard (`ui.rs`).
- The helper speaks the **existing** JSON-lines protocol over the existing named pipe
  (`interprocess` local sockets, `crates/armoury/src/ipc.rs`). It already sends
  `Request::ConsoleHello`. **No new protocol is needed.**
- **Consider moving the SteamGridDB key out of the elevated daemon.** Today
  `cfg.sgdb_api_key` is in the daemon's config and the daemon does the art lookups
  (`daemon.rs:2456`, `daemon.rs:2507`). If the console owns the art code, the key can
  live with it — but a child of an elevated daemon inherits elevation, so a
  *non-elevated* helper needs a linked-token `CreateProcessWithTokenW`. **Decide
  explicitly; do not half-do it.**
