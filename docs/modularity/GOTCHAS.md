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
>
> **Re-measured on the current tree (2026-10-22, `9276196`): the release build is
> 12,808,192 bytes** — that is the v1.1.4 release asset and the installed copy, byte for
> byte (sha256 `a0b19a1c32e05e6511c1a765a02bdd4101d3b9e7ce83e8474d15a3136a45704c`).
> Use this one as the limit.
>
> **After A1 (2026-10-22): 12,868,608 bytes** — **+60,416 bytes (+0.47 %)** for the platform
> verifier, the WinINet proxy reader and the gzip decoder. `DECISIONS.md` §12 says why that
> is accepted, and what would pay it back.
>
> **After A2, on the A2-only tree (`0ecacd3` + A2): 12,925,440 bytes** — **+56,832 bytes
> (+0.44 %)** for the eight new host functions. `DECISIONS.md` §15 has the measurement.
> **After A2, on the branch this phase is delivered on** (the same A2 work on a tree that
> also carries the art fetch through the Windows certificate store): **12,857,344 bytes**,
> which is **+60,928 bytes (+0.48 %)** over *that* tree's baseline of 12,796,416 — the art
> fetch takes Mozilla's bundled roots out of the binary, so that tree starts lower.
> **The baseline for the size rule on the delivered branch is 12,857,344 bytes.** A number
> without its tree is not a number (trap 35).

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
| Resident daemon, **four tracked apps** | **1.72 % of one core** — 1.031 CPU seconds in 60 s (2026-10-22, A1). The difference is the 1 Hz process-table walk (trap 5): with nothing tracked it is skipped. **After A1: 1.692 %** (1.016 CPU s in 60 s) — unchanged, as it has to be. **After A2 (the delivered branch): 1.615 %** (0.969 CPU s in 60 s) — the **median of three 60 s samples**, alternating with the binary before it; the medians are the same, so the phase costs nothing at idle. |

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

1. **The `ureq` root store bug — fixed in A1.** `features = ["rustls"]` only → Mozilla
   roots, not the Windows cert store → the catalog failed behind any corporate CA. The
   fix is the `platform-verifier` feature **and** `RootCerts::PlatformVerifier` together.
   **The art fetches had the same bug and had it fixed as well** — they now share the
   catalogue's client (`art.rs:995` and `console_ui.rs:1324`, both `catalog::agent()`),
   so the SteamGridDB lookups are validated against the Windows certificate store too.
   That fix is **another agent's work** (`fix/art-fetch-windows-root-store`); a tree
   without it still has the bug in the art fetches, and the binary still carries
   Mozilla's bundled roots because of it.
2. **The pack worker is a single serial thread** (`packs.rs:168`,
   `while let Ok(job) = jobs.recv()`). A slow mode would block every later apply and
   revert. Modes need their own worker.
3. **The engine is cached, and the cache is keyed by the capability set** (A2). Since
   1.1.3 the sandbox kept one engine per thread; since A2 it keeps **one engine per
   capability set** (`script.rs:90`, `ENGINES`), built lazily and capped at 32 like
   the AST cache. This **changed how capability scoping had to be built** — it was no
   longer "build an engine per run and register less". See `DESIGN.md` §3.
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
21. **ureq's API is not what it looks like** (learned the hard way in A1).
    `Config` lives at **`ureq::config::Config`**, not at `ureq::Config` — the crate root
    does not re-export it, and the build fails with *cannot find `Config` in `ureq`*.
    `Config::builder()` returns a **`ConfigBuilder`**, not a `Config` — the outer
    `.build()` is missing if you copy a snippet. `ureq::get(uri)` builds a **use-once
    agent** (`Agent::new_with_defaults()`), so there is **no connection pool** at all.
    A **request-level** `.config()` bypasses the agent's pool when the TLS settings
    differ — ureq's own words: *"Request-level changes to connection settings (TLS, IP
    family, TCP_NODELAY, buffer sizes, or the configured User-Agent used for proxy
    CONNECT) bypass the Agent's pool"*. And `RootCerts::PlatformVerifier` **panics at
    runtime** without the `platform-verifier` feature
    (`ureq-3.4.2/src/tls/rustls.rs:184`) — the feature and the setting have to come
    together. The vendored source is in
    `%USERPROFILE%\.cargo\registry\src\…\ureq-3.4.2`: read it, do not remember it.
22. **The WinINet proxy is read when the client is built.** `Config::default()` calls
    `Proxy::try_from_env()`, which reads `HKCU\Software\Microsoft\Windows\CurrentVersion\
    Internet Settings` (`ProxyEnable`, `ProxyServer`). A client that is built once reads
    it once, so a proxy change is only picked up after a restart of the daemon.
23. **A self-signed certificate cannot serve TLS on Windows.** `SslStream` refuses an
    ephemeral key: *"Authentication failed because the platform does not support
    ephemeral keys"*. A local HTTPS probe needs a key in the user's key store, so the
    harness probes the public catalogue instead.
24. **`Cargo.lock` carries two `windows-sys` versions now**: 0.61.2 (the project's) and
    0.52.0 (from `rustls-platform-verifier`). That is normal — do not "fix" it.
25. **Rhai's "unknown function" is also what a wrong argument type produces.** A call
    to a function that is on the engine but with the wrong arguments comes back as
    `ErrorFunctionNotFound` too — exactly like a call to a function that is not on the
    engine at all. The capability gate reads the function name out of that error
    (`script.rs:265`, `denial`), so it **must** check that the capability really was
    not granted before it calls the call a denial. A pack that was granted `exec` and
    called `ctx.run_cli(42)` must not be told it needs the `exec` capability.
26. **Rhai names a call by its signature, not by its name**: the error carries
    `"run_cli (ScriptHost, &str | ImmutableString | String)"`. The name has to be cut
    off at the first space before it can be looked up in the capability table.
27. **An AST belongs to no engine.** The compiled script is resolved against the
    engine when it *runs*, so one AST cache serves every capability set — only the
    engine is per capability set. A pack's script may therefore be compiled by the
    daemon's main thread (the install-time check) and run on the pack worker's
    restricted engine.
28. **COM is per thread.** The daemon's main thread has an apartment
    (`killswitch::init_com()`), but the **pack worker is a thread of its own** — a pack
    that asks for the `audio` capability needs one too, or every audio call fails.
    `packs::start` sets it up.
29. **A window belongs to the thread that created it.** The message queue of the
    overlay's windows lives on the thread that created them, so a pack worker cannot
    show the idle overlay itself: the daemon's main loop owns it, and the host
    function hands the request over.
30. **The COM interfaces of the `windows` crate are not `Send`.** A global ledger
    cannot hold the audio session objects (the killswitch's own `Ledger` is a plain
    struct on the main thread for that reason). The host's ledger therefore holds the
    **session identifier** — a string — and matches the sessions of a sweep by it.
31. **Count the tests line by line, not with a loose regex.** The scan that counts the
    tree's tests has to be anchored to the **end of the line**
    (`(?m)^[ \t]*#\[test\][ \t]*$`): a pattern with a `\s*` that may cross a newline
    counts **four tests too many**, and the number then disagrees with what CI reports.
    The same goes for the harness's `Check` calls (`^[ \t]*Check[ \t]`). The honest
    numbers for the A1 tree are **341 unit tests** (256 in `armoury`, 85 in
    `armoury-core`) and **90 harness checks** — the ones the folder already carries.
32. **`HashMap::new()` is not a `const fn`.** A global ledger that is a map cannot be
    built in a static's own initializer (`Mutex::new` and `Vec::new` are const, a
    `HashMap` is not) — use a `OnceLock` and build the map in `get_or_init`.
33. **A Rhai host function is not free: about 5 KB of machine code per *shape*.**
    Every shape of registered function gets its own instantiation of the
    argument-marshalling code — the `Dynamic` unboxing of each parameter and the
    wrapping of the return value. Eight host functions with six shapes cost the
    executable **47,616 bytes of `.text`** (measured: the section tables of the two
    release builds). A "thin wrapper" over a Win32 call is therefore not thin in the
    binary, and the host API's size is worth watching before it grows again.
34. **The PE section table tells you where the bytes went.** The release build is a PE
    file; reading the section headers (`.text`, `.rdata`, `.pdata`, …) of two builds
    says in one minute whether a change grew *code* or *data*. That is how the
    registration cost above was found.
35. **The numbers in this folder are per tree, and a shared tree is not the A2 tree.**
    The branch this work is delivered on carries another agent's commits (the art
    fetch through the Windows certificate store), so it has **four more unit tests**,
    **five more harness checks** and a **different executable size** than the A2-only
    copy of the same phase. Quote the numbers of the tree you actually measured, and
    say which tree that is. A number without its tree is not a number.
36. **The size baseline moves when another branch lands — and it can move *down*.**
    The art fetch took Mozilla's bundled roots out of the binary, so the release exe
    went from 12,868,608 bytes (A1) to **12,796,416** without anybody touching the
    size rule. Re-measure the baseline on the tree you are on; never carry a baseline
    over from another branch.
37. **`HashMap::new` is not a `const fn` because the map is randomly seeded.** The
    seed is the HashDoS protection, and a seed cannot be made at compile time — so
    `static M: Mutex<HashMap<..>> = Mutex::new(HashMap::new())` does not compile.
    `HashMap::with_hasher(BuildHasherDefault::new())` *is* const (since 1.85), but it
    has a **fixed** hash: do not "fix" the ledger with it. Use a `OnceLock` (or
    `LazyLock`) and build the map in `get_or_init`. (`Mutex::new` and `Vec::new` are
    const, so a `Vec` in a static is fine.)
38. **A window handle outlives its window.** `ShowWindow` on a handle whose window was
    closed does nothing, and Windows may hand the same handle to another window later.
    A ledger of "the visibility this handle had before we hid it" is therefore wrong
    after the window is gone: check `IsWindow` before you report the old state, or a
    revert shows a window it should not.
39. **One 60-second idle sample of the daemon proves nothing.** The samples of one and
    the same binary scattered over **0.812–1.016 CPU s in 60 s** (1.35 %–1.69 % of a
    core) on this machine, with the user's own desktop running. Measure **three
    samples of each binary, alternating**, and compare the medians. A single sample
    can make a change look like a 0.2 % swing that is really the noise.

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
   (`script.rs:668`, `write_allowed`) compares path *components*, not string
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
