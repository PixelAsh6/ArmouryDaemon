# Gotchas, measurements and house rules

Everything here was measured or read out of the source. **Do not rediscover it.**

---

## 1. The house rules

### Build in CI, never on this machine

**All building happens in GitHub Actions.** Do not run `cargo build`, `cargo test` or
`rustc` on this machine — not even a parse-only invocation. The release profile takes
13–21 minutes of the user's CPU; a runner does it for free and identically.

- Verify by pushing and watching CI: `gh run watch` (the `gh` CLI is installed and
  authenticated as PixelAsh6).
- **A release binary comes from the release workflow's artefact, never from a local
  build.**
- The CI workflow (`.github/workflows/ci.yml`) builds the workspace, checks the
  executable carries its manifest, runs the unit tests, checks the shell is elevated,
  and runs the self-test harness. A release build happens on a tag push **or on a
  manual run** (`workflow_dispatch`), and that run leaves the release build and its
  sidecar behind as an artefact, so a binary can be tested before a release exists.
- Both workflows pin the toolchain to **1.98.0**, not `stable`: `stable` moves every
  six weeks, and the tested binary has to be the published one.

### Commit authorship is the user's

Author **and** committer must both be
`PixelAsh6 <PixelAsh6@users.noreply.github.com>`. No AI identity anywhere — not in
commit messages, code, comments or docs.

### Never run `cargo fmt`

The repo has no rustfmt pins and `cargo fmt` reflows the whole repository. Format by
hand.

### Keep the promise

"Everything is local, no telemetry." No crash reporting, no analytics.

### Note what you find

If you hit a trap, write it in this file immediately, while you still have the context.
That is the entire reason the file exists.

---

## 2. Measured on this machine

Ryzen 7 4800H (8C/16T), 15.42 GiB RAM, Windows 11 26300, 125 % DPI, rustc 1.98.0,
Defender real-time protection **on**. 21 samples/case, median + nearest-rank p95.

### Binary size (real release builds, `lto="fat"`, `codegen-units=1`)

| Variant | exe bytes | Δ vs full | % removed |
|---|---|---|---|
| **full** | **12,839,424** | — | — |
| − eframe | 7,196,672 | −5,642,752 | 43.9 % |
| − rhai | 9,762,304 | −3,077,120 | 24.0 % |
| − ureq | 10,930,688 | −1,908,736 | 14.9 % |
| − image | 12,312,576 | −526,848 | 4.1 % |
| **− all four** | **2,125,824** | −10,713,600 | **83.4 %** |

`cargo bloat --release -p armoury --crates` on the `.text` (9.6 MiB of the 12.2 MiB
file): `std` 1.9 MiB (20 %), **`rhai` 1.7 MiB (17.6 %)**, own code 975.6 KiB (9.9 %),
`egui` 786.0 KiB (8.0 %), `rustls` 470.7 KiB (4.8 %), `harfrust` 392.0 KiB,
`skrifa` 333.8 KiB, `armoury_core` 233.8 KiB, `epaint` 217.1 KiB, `ureq` 204.6 KiB.
Families: GUI ≈ 2.56 MiB, scripting ≈ 1.73 MiB, HTTP/TLS ≈ 904 KiB, images ≈ 346 KiB.
Biggest symbols: `daemon::run` 247.2 KiB, `console_ui::ui` 92.5 KiB,
`rhai::packages::arithmetic::init` 67.6 KiB, `script::engine` 33.4 KiB.

> **These numbers predate two changes**: `strip` is now `"debuginfo"` (not `true`) and
> the target moved to `x86-64-v3`. **Re-measure the baseline before using the byte
> count as a limit.**

### Runtime

| Operation | Cost |
|---|---|
| Process spawn (spawn → running) | **35.87 ms** (6.57 ms `CreateProcess` + 29.07 ms child startup/Defender scan) |
| eframe GUI module → first presented frame | **593 ms**, **69.5 MiB** peak working set |
| Rhai script, warm | **4.7 µs** on the engine (1.1.3); eval 49 µs; a host call ~2 µs |
| `EnumWindows` (528 windows) | **340 µs** |
| Core Audio full sweep | **5.48 ms** |
| Named-pipe JSON round trip | **28.0 µs** (50 sequential: 32.5 µs/trip) |
| SHA-256 | 1,312 MiB/s (32 MiB in 24.18 ms) |
| Resident daemon | 2.0 MB private / 12.7 MB WS / 0.078 % of one core (measured pre-1.1.3) |

### What the numbers decide

1. **The process boundary is the expensive mechanism; everything in-process is cheap.**
   36 ms + 70 MiB per GUI module, versus microseconds behind the pipe.
2. **Linking eframe/rhai/interprocess into the daemon costs ~1 MiB of RSS** (9.5 MiB
   linked-without-a-window vs 8.3 MiB bare). Splitting the GUI out of the *resident*
   daemon buys almost nothing in RAM — only disk bytes.
3. **Rhai is 3 MB of a 12.8 MB binary** — 24 % of the executable, for one shipped pack.
4. **"Compile native for the user's CPU" is worth ~0–5 % here and is unmeasurable.**
   The workload is a 1 Hz process walk, some Win32 calls, JSON parsing, and a
   GPU-bound UI (35 µs of CPU per frame in the `ui` callback, 6.1 ms steady frame
   interval). You would charge a Rust toolchain install and 15 minutes of build time
   for a number nobody can see. The build already targets `x86-64-v3`.

---

## 3. The traps

1. **The `ureq` root store bug.** `features = ["rustls"]` only → Mozilla roots, not the
   Windows cert store → the catalog fails behind any corporate CA. **Still open.**
   (`WORK-ORDER.md` A1.)
2. **The pack worker is a single serial thread** (`packs.rs:160`,
   `while let Ok(job) = jobs.recv()`). A slow mode would block every later apply and
   revert. Modes need their own worker.
3. **The engine is cached per thread** since 1.1.3 (`script.rs:86`, `SANDBOX`). This
   **changes how capability scoping must be built** — it is no longer "build an engine
   per run and register less". See `DESIGN.md` §3.
4. **The killswitch no longer re-sweeps audio per pid** (1.1.3). One sweep per press;
   the release restores from the ledger without enumerating at all.
5. **The daemon still walks the whole process table every second when something IS
   tracked** (`watch.rs:174` `Watcher::step` → `enum_processes` at `:428`). The 1.1.3
   fix skips the walk only when **nothing** is tracked. The design docs
   (`project-map.md`, `README.md`) describe a *"tiered 1 s cheap probe → full scan on
   change"* — **the word "tier" appears nowhere in the code.** Either implement the
   tiering or correct the docs. Do not leave them contradicting each other. This is
   the daemon's remaining idle cost and **no module architecture changes it**.
6. **`id == "rainmeter"` hardcoding** (`daemon.rs:733`) plus
   `packs::rainmeter_layouts()` (`packs.rs:795-814`) is the last per-pack special
   case in the daemon.
7. **The killswitch has no config key and no IPC request** — it cannot be disabled at
   all.
8. **The shipped exe imports `VCRUNTIME140.dll`** — not a truly standalone file.
9. **A fat-LTO release build takes 13–21 minutes.** A build that looks hung is
   linking. Use CI.
10. **`target/` is 2.85 GB / 1264 files**, and deleting it right after a build fails
    intermittently because Defender holds handles on the new binaries. Copy first,
    verify, then delete with retry-with-backoff.
11. **The manifest's `file` path is repository-relative, not manifest-relative.** This
    caused a real 404 in testing. Do not "fix" it.
12. **The build already targets `x86-64-v3`**, not `native` (`.cargo/config.toml`).
    Do not change it back.
13. **The lockout model was rewritten twice**: on 2026-10-21 and again on 2026-10-22.
    It now **holds a change in both directions**. A start with **no window running**
    applies at once and starts a window of its own; a start **inside** a window is
    **held** and lands at the expiry — if the app is still running by then —
    **exactly once**; a close is **always held** (it starts a window if none is
    running and keeps the running one rather than restarting it); a **restart inside
    the window cancels the hold**; a **manual action bypasses** the window. Read
    `engine.rs:291` (`tick`), `:314` (`try_change`), `:366` (`lock_running`), `:145`
    (`set_paused`), `:183` (`set_idle`) before touching it.
    The 2026-10-21 model — *"the apply is never deferred, the lockout holds the
    revert"* — is **rejected**: it was an over-correction, it was not needed for the
    21 Oct flip-flop, and the user rejected it on 2026-10-22. The older model —
    "the lockout is a minimum spacing between two changes" — is gone for good.
    **The trap:** a start inside a window means the packs are *late*. That is the
    intended behaviour, not a bug — do not "fix" it by applying at once.
14. **`/docs/plans/` and `/docs/research/` are gitignored**, along with `HANDOVER.md`,
    `PLAN.md`, `RESEARCH.md`, `PACK-SPEC-PROPOSAL.md`, `SPEC3-SCRIPTING.md`,
    `DECISIONS-2026-10-17.md` and **`/docs/scope.md`** — the private scope note for
    the orchestrator and the user. They stay on disk and are not published.
    **`docs/modularity/` is versioned** — that is why this folder is here.
15. **There is still no stable Rust ABI in 2026.** RFC 3435 (`#[export]`) is open with
    tracking issue `S-tracking-unimplemented`; RFC 3470 (crABI) is open; RFC 3930 was
    rejected in March 2026; `abi_stable` has been unmaintained since October 2023 and
    Apache DataFusion migrated off it in April 2026. **Do not build a DLL-plugin
    system.**
16. **MSIX is disqualified**: it forbids per-user Windows services, blocks HKLM
    writes, forbids elevation, and states that modules cannot be loaded into processes
    outside the package.
17. **`tough` does not support delegated roles** (TAP 3/4) and the `tuf` crate is
    beta. minisign now; TUF later, if third-party publishers ever appear.
18. **The self-test harness needs an elevated shell** (CI asserts this). It launches a
   throw-away daemon with an env-namespaced pipe/config/mutex. **Read it before
   changing anything it asserts** — it has already caught two real bugs.
19. **The packs repo is a separate repository** — `D:\ArmouryDaemon_Packs` (GitHub
    `PixelAsh6/ArmouryDaemon_Packs`), manifest `catalog/index.json`. A change to the
    pack format needs a change there too, and the release workflow checks the shipped
    packs against that catalogue byte for byte.
20. **The release carries two assets and no more**: `ArmouryDaemon.exe` and its
    85-byte `ArmouryDaemon.exe.sha256` sidecar. There is **no zip** any more and
    **no scripts** in it — the executable installs itself, and the scripted install
    lives in the repository's `deploy\` folder. Anything that plans a download of
    `ArmouryDaemon-<version>-windows-x64.zip` is planning a file that does not
    exist. The release workflow still checks the repo's `packs\` folder against the
    catalogue, but it no longer ships those bytes.

---

## 4. Security — the constraint that shapes everything

**The daemon runs elevated.** `deploy/setup.ps1` registers a scheduled task with the
highest privileges that starts the exe at logon and on resume from sleep. The install
folder's ACL gives **Administrators + SYSTEM full, the interactive user Modify,
everybody else read + execute**.

**Therefore: a downloaded helper EXE that the elevated daemon launches is code
execution as Administrator.** This is the single most important constraint in the whole
project.

Required defences:

1. **The manifest is signed**, and the signature is verified **before** the manifest is
   parsed. The payload's SHA-256 and size are checked after download. Nothing that
   fails a check is ever written to disk in a location the daemon will execute from.
2. **The module directory gets its own ACL** — Administrators + SYSTEM full, the user
   read + execute, **no Modify**. The daemon must not execute from a directory the
   user can write to. `deploy/setup.ps1` already does this for the install folder; it
   needs the same treatment for `modules/`.
3. **The helper runs non-elevated where possible.** A child of the elevated daemon
   inherits elevation; a linked-token `CreateProcessWithTokenW` is the way to drop it.
4. **The capability gate is the real boundary** for data modules. A pack that declares
   `["windows","audio"]` gets those and nothing else; an ungranted call must fail.
5. **A pack script may not write outside** `%APPDATA%`, `%LOCALAPPDATA%`,
   `%PROGRAMDATA%` and the target app's own folder — the existing guard
   (`script.rs:471-533`, `script.rs:610`) compares path *components*, not string
   prefixes, and refuses symlinks. Do not weaken it. Note that
   `%APPDATA%\ArmouryDaemon` is inside `%APPDATA%`, so a pack script can already
   write `config.json` and `journal.json` — decide deliberately whether that stays
   true.
6. `run_cli`, `process_start` and `process_kill` are already restricted to the same
   folders `write_file` may write to. `cmd.exe` and `powershell.exe` live in
   `System32` and are out of reach. Do not widen that.

---

## 5. Environment

- Repo: `D:\Armoury Daemon` (the path has a space — cargo is fine with it). Remote
  `https://github.com/PixelAsh6/ArmouryDaemon.git`, branch `main`.
- `gh` CLI 2.102.1, authenticated as PixelAsh6. `gh run watch`, `gh run list`,
  `gh pr create`.
- Toolchains installed locally: `stable` 1.98.0 (active), `nightly`, 1.70.0, 1.98.0,
  1.98.1. Only `x86_64-pc-windows-msvc` is installed — `wasm32-wasip2` must be added
  for Phase E.
- `wasm-tools`, `wit-bindgen` and `wasmtime` are **not** installed.
- crates.io, static.crates.io, index.crates.io and github.com are reachable.
- Disk: 124 GB free on D:.
- CI caches `~/.cargo/registry`, `~/.cargo/git` and `target`, keyed on `Cargo.lock`
  **and** `.cargo/config.toml` — so editing `.cargo/config.toml` invalidates the
  cache.
- New test helpers: `crates/armoury-core/src/testutil.rs` and
  `crates/armoury/src/testutil.rs`, each with a `Scratch` type.
