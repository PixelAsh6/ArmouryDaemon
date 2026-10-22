# Progress

The running log. **Append to it at the end of every phase**, and update the block at
the top. Write it for someone with no memory of the session: **state, next step, and
anything surprising you found.**

---

## Current state

| | |
|---|---|
| **Phase** | A — **A1 and A2 done** (the TLS root store, the capability-scoped host API), A3 next |
| **Next action** | **A3** — `triggers` + `kind` in the pack schema (`WORK-ORDER.md` A3) |
| **Branch** | `main` is at `0ecacd3` (version **1.1.4**). The A2 work is delivered on **`feature/a2-capabilities`** — on top of `0ecacd3` **and** the art fetch through the Windows certificate store, which is another agent's work. The **A2-only** copy of the phase is on `feature/a2-capabilities-clean`; it does **not** carry the fixes of the entry below, so the two branches have different numbers |
| **Tree** | clean |
| **Blocking** | nothing |
| **Open questions** | none — the design is settled in `DECISIONS.md` |

---

## Log

### 2026-10-22 — the working folder was created

The modularity research and design were finished and the plan was written to
`docs/plans/2026-10-21-modular-modules.md` (on disk, gitignored). This folder is the
**versioned, operational** copy, split by topic so it can be re-read cheaply after an
auto-compaction.

Nothing was implemented. The tree is unchanged.

**Surprises worth remembering:**

- `/docs/plans/` and `/docs/research/` are **gitignored** — the project's specs
  (`PACK-SPEC-PROPOSAL.md`, `SPEC3-SCRIPTING.md`, `HANDOVER.md`) live on disk and are
  not published. `docs/modularity/` is *not* ignored, which is why the folder is here
  and not there.
- The lockout model was rewritten in 1.1.3 and the old docs still described the old
  one. The new model: **the apply is immediate, the lockout holds the revert**, a fast
  restart cancels the hold, and a manual action bypasses a running lockout.
  **Superseded on 2026-10-22** — that model was an over-correction and the user
  rejected it. See the entry below and `DECISIONS.md` §10.
- `Engine::set_idle` is *already* a mode in the engine's own words ("A mode change is
  deliberate: it drops a running lockout and starts none"). **Modes need no new engine
  state** — `suspends_packs` drives `set_idle` and `react_to_tracked` drives the
  existing daemon-side re-engage at `daemon.rs:1334-1339`.
- The build already targets **`x86-64-v3`**, not `native` (`.cargo/config.toml`), so
  the "compile native for the user's CPU" idea is already resolved in the direction
  that makes the build portable and CI-producible.
- 1.1.3's three performance fixes moved the idle-cost baseline, so the numbers in
  `GOTCHAS.md` must be re-measured before they are used as limits.

---

### 2026-10-22 — the lockout was corrected, and the release shape changed

**What I did.** The lockout model was rewritten a second time, so this folder had to
follow: it still described the 21 Oct model ("the apply is immediate, the lockout
holds the revert"), which the user has now rejected. `GOTCHAS.md` §3.13 was
rewritten for the model the code implements, and its line anchors were re-read off
the file (`tick` `:291`, `try_change` `:314`, `lock_running` `:366`, `set_paused`
`:145`, `set_idle` `:183`). `DECISIONS.md` gained §10, which records the corrected
model and what was rejected and why. The release now carries **only the executable
and its 85-byte sidecar** — no zip, no scripts — and both workflows pin the
toolchain to **1.98.0** instead of `stable`.

**Files touched.** `crates/armoury-core/src/engine.rs`, `tools/daemon-selftest.ps1`,
`Cargo.toml`, `Cargo.lock`, `crates/armoury/assets/ArmouryDaemon.manifest`,
`README.md`, `deploy/README.md`, `CHANGELOG.md`, `.github/workflows/ci.yml`,
`.github/workflows/release.yml`, `.gitignore`, and this folder.

**Measured.** The tree carries **339 `#[test]` functions** — **254** in `armoury`,
**85** in `armoury-core` — counted with a static scan of the `#[test]` attribute,
and the harness has **84** `Check` calls, counted the same way. The tree carried
335 (254 / 81) and 82 checks before this work, so the rewrite added **4 tests and 2
checks**. The four new tests are the ones the rewrite brought: a change inside a
window is held in both directions, a held apply is cancelled when the app closes
again, a held apply lands exactly once, and a close between two windows is still
held.

**Decisions.** The lockout holds a change in **both** directions (`DECISIONS.md`
§10). The 21 Oct model is rejected: it was an over-correction — the 21 Oct
flip-flop was fixed by holding the close, and never needed the apply to be exempt
from the window. A start with no window running still applies at once, and a
manual action still bypasses the window.

**Surprises.** The folder's line anchors were **stale**: it cited `set_paused` at
`:136` and `tick` at `:282`, and the file has them at `:145` and `:291`. The
`set_idle` anchor in `DECISIONS.md` §2 was stale in the same way (`:174` →
`:183`). The harness has **84** `Check` calls, not 83: the scan is `^\s*Check\s`,
and all 84 are real calls. `README.md`'s install
section pointed at `ArmouryDaemon-1.1.3-windows-x64.zip`, a file the release no
longer publishes, so the section now describes the single-executable path and the
scripted install in the repository's `deploy\` folder.

**Next.** Phase A1 — the TLS root store fix. `Cargo.toml` has `ureq = { ...,
features = ["rustls"] }` only, so the catalogue fetch validates TLS against
Mozilla's bundled roots instead of the Windows certificate store. Add the
`platform-verifier` and `win-system-proxy` features, build the `TlsConfig` with
`RootCerts::PlatformVerifier`, extend the self-test harness if the catalogue fetch
can be exercised offline, and note it in the changelog. Nothing in this folder
blocks it.

---

### 2026-10-22 — Phase A1: the catalogue is validated against the Windows certificate store

**What I did.** The catalogue fetch validated TLS against Mozilla's bundled roots, so it
failed on any machine with a corporate CA or with TLS interception. It validates against the
**Windows certificate store** now (`RootCerts::PlatformVerifier`), it reads the user's
**WinINet proxy settings**, and it asks for a **compressed** response. The client is **one
`ureq::Agent`**, built on the first fetch and shared by every fetch: a request-level TLS
change bypasses the agent's connection pool, and the rustls config and the platform verifier
are built once instead of once per request (`DECISIONS.md` §11).

**Files touched.** `Cargo.toml` (the `ureq` features), `Cargo.lock` (ten packages added),
`crates/armoury/src/catalog.rs` (the client and two unit tests), `tools/daemon-selftest.ps1`
(six checks), `CHANGELOG.md`, `README.md`, and this folder.

**Measured.**

| | before | after |
|---|---|---|
| The release exe | **12,808,192 bytes** (v1.1.4, `9276196`) | **12,868,608 bytes** — **+60,416 (+0.47 %)** |
| The daemon's idle cost | **1.031 CPU s in 60 s = 1.719 % of one core** | **1.016 CPU s in 60 s = 1.692 % of one core** — unchanged |
| The unit tests | 339 (254 in `armoury`, 85 in `armoury-core`) | **341** (256 / 85), all green in CI |
| The harness checks | 84 `Check` calls | **90 `Check` calls**, all green in CI |

*Idle cost:* a throw-away instance — its own pipe, config folder and single-instance mutex
(`ARMOURY_TEST_NS`), started with `--autostart` so no dashboard opens — with the three
shipped packs and four tracked apps, which is the shape of the installed daemon. Twenty
seconds of settling, then sixty seconds of `TotalProcessorTime`. **The installed daemon was
not stopped, started or restarted.**

*Exe size:* the release build of the tree, taken from the CI artefact. `cargo build
--release` is never run on this machine. **The exe grew by 60,416 bytes** — the platform
verifier, the WinINet proxy reader and the gzip decoder. `DECISIONS.md` §12 says why that is
accepted, and what would pay it back (the Mozilla roots that are still in the binary for the
art fetches' sake).

*The CI run:* `39696490825` (the workflow dispatch on the measurement branch), the job green
in **8 min 11 s**: the debug build, **341 unit tests**, and the self-test harness with
**90 checks, all passed**.

**Decisions.** `DECISIONS.md` §11: the catalogue client is one agent, built once, with the
platform verifier. Rejected: a request-level `.config()` (it bypasses the pool), one agent
per request (a new rustls config and a new platform verifier per fetch), and Mozilla's roots
(the bug). `DECISIONS.md` §12: the phase adds **60,416 bytes** to the executable, and that
is accepted — the size rule bends for a fix that makes the catalogue reachable.

**Surprises.**

- The folder's snippet was wrong in two ways, and the first one would not have compiled:
  `Config::builder()` returns a **`ConfigBuilder`**, not a `Config` — the outer `.build()`
  was missing — and `ureq::get(url).config()` is a **request-level** config, which ureq's own
  documentation says bypasses the agent's connection pool. The ureq source is in the cargo
  registry (`%USERPROFILE%\.cargo\registry\src\…\ureq-3.4.2`); read it, do not remember it.
- **`Config` is at `ureq::config::Config`**, not at `ureq::Config` — the crate root does not
  re-export it. The first CI build of this phase failed with *cannot find `Config` in
  `ureq`*, and that is the only thing the build caught: the rest of the change was right
  the first time.
- `ureq::get(uri)` builds a **use-once agent** (`Agent::new_with_defaults()`), so the old code
  had **no connection pool at all** — and it would have rebuilt the platform verifier for
  every single fetch.
- The root store is not a ureq feature: `RootCerts::PlatformVerifier` **panics at runtime**
  without the `platform-verifier` feature (`tls/rustls.rs:184`). The feature and the setting
  have to come together, or the daemon panics on the first catalogue fetch.
- `win-system-proxy` reads `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet
  Settings` (`ProxyEnable`, `ProxyServer`) — the WinINet settings — and it is read by
  `Config::default()`, so the proxy is read **when the client is built**. A proxy change is
  picked up after a restart of the daemon.
- The catalogue responses are compressed now, and the bytes that are verified against the
  manifest are still the plain ones: ureq strips `Content-Encoding` and `Content-Length` and
  hands the decoded body over.
- **The art fetches still use Mozilla's roots.** `art.rs:991` and `console_ui.rs:1320` have
  their own `ureq::get(...)` calls with no TLS config, so the SteamGridDB lookups are still
  validated against the bundled roots. They were out of scope for A1 — the phase is the
  catalogue — but it is the same bug and it is **still open**. It is also why the binary
  grew: the **Mozilla roots are still in it** (they are only used by those two calls), and
  moving them to the shared client would take them out again.
  **Superseded on 2026-10-22 (the same day, later):** the art fetches now share the
  catalogue's client (`art.rs:995`, `console_ui.rs:1324`), so they are validated against
  the Windows certificate store too, and the bundled roots are out of the binary. That is
  **another agent's work** (`fix/art-fetch-windows-root-store`), and it is why the size
  baseline on this branch is *lower* than the A1 one — see the entry at the end of this
  file and `GOTCHAS.md` trap 36. On a tree without that commit, the bullet above still
  holds.
- A **self-signed certificate cannot be used for a local TLS server** on Windows: `SslStream`
  refuses it with *"the platform does not support ephemeral keys"*. A probe that shows the
  daemon **refusing** an untrusted certificate is therefore not possible without a key in the
  user's key store. The harness probes the public catalogue instead.
- `Cargo.lock` now carries a **second `windows-sys`** — 0.52.0, from
  `rustls-platform-verifier` — next to the 0.61.2 the project uses.
- The harness now has a **local server** (a loopback `TcpListener` it answers from) so the
  fetch path can be tested offline. Run against the **old** binary, the three new checks
  fail exactly as they should: the old client asks for no compression and cannot read a gzip
  body ("stream did not contain valid UTF-8"). That is how a new check is shown to be a new
  check.

**Next.** Phase A2 — the capability-scoped host API. `DESIGN.md` §2 has the capability
table and §3 the mechanism: **one engine per capability set** (the sandbox keeps one engine
per thread since 1.1.3, so "register less" is no longer free), built lazily in a thread-local
map capped like the AST cache. A pack that calls a function it was not granted has to fail as
an **unknown function**, and the denial has to name the missing capability and land in the
pack's log. `WORK-ORDER.md` A2.

---

### 2026-10-22 — Phase A2: the host API is scoped by capability

**What I did.** A pack declares the **capabilities** it wants and the daemon grants only
those. The sandbox now keeps **one engine per capability set** — built lazily, capped at
32 like the AST cache, keyed by a bitmask over the twelve capabilities — and registers
**only the granted functions** on it. A call to a function the pack was not granted fails
as an **unknown function**: the denial is structural, not a check a call site could
forget, because there is no call site to forget it at.

The taxonomy lives in the schema crate (`armoury_core::pack::Capability` and `CapSet`),
because the permission window (Phase B4) has to show the same table the gate uses. The
pack schema gained an **additive** `capabilities` field that defaults to every
capability, so the three shipped packs keep working unchanged.

The eight host functions the table listed as *new* now exist: `windows_of`,
`window_hide`, `window_show` (the killswitch's own windows), `audio_sessions`,
`audio_mute`, `audio_mute_all` (the audio sessions and the output devices) and
`overlay_show`, `overlay_hide` (the idle overlay) — **eight** functions over the 18
that were there. They are thin wrappers over the code that was already there —
**nothing moved**.

**Files touched.** `crates/armoury-core/src/pack.rs` (the capability taxonomy, the
`capabilities` field, six tests), `crates/armoury/src/script.rs` (the engine cache, the
per-capability registration, the denial, six tests), `crates/armoury/src/packs.rs` (the
grant set reaches the worker, the window and audio helpers, the overlay request queue),
`crates/armoury/src/daemon.rs` (the overlay requests), `crates/armoury/src/killswitch.rs`
(the session ids, `windows_of` made public, `session_states`, `mute_pid`),
`crates/armoury/src/overlay.rs` (`mute_all_host`), `tools/daemon-selftest.ps1` (five
checks) and this folder.

**Measured.**

| | before | after |
|---|---|---|
| The release exe | **12,868,608 bytes** (A1, `0ecacd3`) | **12,925,440 bytes** — **+56,832 (+0.44 %)** |
| The daemon's idle cost | **1.016 CPU s in 60 s = 1.692 % of one core** (A1, the same tree) | **0.984 CPU s in 60 s = 1.641 %** — no regression |
| The unit tests | 341 (256 in `armoury`, 85 in `armoury-core`) | **353** (262 / 91) — **+12** |
| The harness checks | 90 `Check` calls | **95** `Check` calls — **+5** |

*Idle cost:* the A1 method, unchanged. A throw-away instance — its own pipe, config
folder and single-instance mutex (`ARMOURY_TEST_NS`), started with `--autostart` so no
dashboard opens — with the three shipped packs and four tracked apps, which is the shape
of the installed daemon. Twenty seconds of settling, then sixty seconds of
`TotalProcessorTime`. **The installed daemon was not stopped, started or restarted**, and
none of the four tracked apps was running, so no pack ran and nothing on the machine
moved.

*Exe size:* the release build of the tree, taken from the CI artefact.
`cargo build --release` is never run on this machine. **The executable grew by 56,832
bytes.** `DECISIONS.md` §15 says where the bytes are (a Rhai registration costs about
5 KB of machine code per *shape*, and the phase adds eight host functions over six new
shapes) and what was rejected. **The size rule bends here, once** — and the baseline
for the next phase is now 12,925,440 bytes.

*The CI run:* **39707130123** (the workflow dispatch on the measurement branch), the job
**green** in **9 min 31 s**: the debug build, **353 unit tests**, and the self-test
harness with **95 checks, all passed** — plus the release build and the artefact the
size was measured from. A later run on the branch's tip was green as well
(**39708602555**, **7 min 48 s**): the same 353 tests, the same 95 checks, and the same
**12,925,440-byte** executable.

**Decisions.** `DECISIONS.md` §14: the taxonomy, one engine per capability set, the
**`time` capability for `sleep`**, the default of the `capabilities` field, and the
overlay as a request to the daemon rather than a call from the pack.

**Surprises.**

- **The table had a hole in it.** It listed 17 of the 18 existing host functions:
  **`sleep` was in no capability at all**. Under per-capability-set engines a pack whose
  script called `sleep` would have failed as an unknown function. `sleep` is now a
  capability of its own (`time`): it grants nothing, it is a flow-control primitive, and
  the operation budget and the wall clock already bound it. Folding it into `context`
  would have handed a read-only pack a way to stall the pack worker.
- **`DESIGN.md` §1 said the default of `capabilities` was `[]`.** That is wrong for the
  shipped packs — the Rainmeter pack's script uses `window_message` — so the default is
  **every capability** and the field is what the pack *asks* for. The user's own grant
  list comes later and narrows it down. `DESIGN.md` §1 now says so.
- **Rhai's "unknown function" is also what a wrong argument type produces.** A call to a
  function that *is* on the engine but with the wrong arguments comes back as
  `ErrorFunctionNotFound` too. The gate therefore checks that the capability really was
  not granted before it calls the call a denial — otherwise a pack that was granted
  `exec` and called `ctx.run_cli(42)` would be told it needs the `exec` capability.
- **Rhai names a call by its signature**: the error carries
  `"run_cli (ScriptHost, &str | ImmutableString | String)"`, so the name has to be cut
  off at the first space.
- **An AST belongs to no engine** — the functions are resolved when it *runs* — so one
  AST cache serves every capability set. Only the engine is per capability set.
- **COM is per thread.** The daemon's main thread has an apartment; the **pack worker is
  a thread of its own**, and a pack that asks for the `audio` capability needs one too.
- **A window belongs to the thread that created it.** The pack worker therefore cannot
  show the idle overlay: the request is handed to the daemon's main loop, which keeps
  the crash-recovery ledger, the keypress dismissal and the idle-mode policy.
- **The COM interfaces of the `windows` crate are not `Send`**, so a global ledger cannot
  hold the audio session objects. The ledger holds the **session identifier** instead —
  Windows gives every session one, and it survives from sweep to sweep.
- **`HashMap::new()` is not a `const fn`**: a global map cannot be built in a static's
  own initializer. The ledger is a `OnceLock` and the map is built in `get_or_init`.
- **`IAudioSessionEnumerator::GetCount` returns an `i32`**, not a `u32` — the index of a
  session in the sweep is an `i32`. Two CI builds found this.
- **The host API has 26 functions, not 25**: the table's three new groups carry
  **eight** functions (3 + 3 + 2), not seven. A unit test counts them, and it caught
  the miscount in the first CI run.
- **The executable grew by 56,832 bytes** (+0.44 %) and the size rule says only Phase E
  may add bytes. The bytes are the eight new host functions: a Rhai registration costs
  about 5 KB of machine code per *shape*, and the phase adds six new shapes.
  `DECISIONS.md` §15 has the measurement, the reason and the alternative (the eight
  functions can move to the killswitch/idle phase, B/C, if the user wants the size
  flat).
- **Two agents share this working tree.** The A2 branch picked up the other agent's
  commits, so an A2-only copy was made on `feature/a2-capabilities-clean`, built from
  `0ecacd3` with only the A2 commits on it. **The phase was later finished on the
  shared branch `feature/a2-capabilities`**, which is the branch it is delivered on —
  see the entry below, and note that the two trees have different numbers.

**Next.** Phase A3 — `triggers` + `kind` in the pack schema (`crates/armoury-core/src/
pack.rs`). The pack gains a `kind` (`"pack"` or `"mode"`) and a `triggers` list, both
**additive**: the three shipped packs must keep loading, installing, applying, reverting
and removing unchanged, and a malformed `kind` has to be a named error like the other
schema errors. `WORK-ORDER.md` A3. The capability table is in `DESIGN.md` §2 and the
`Capability`/`CapSet` types are already in `armoury_core::pack` — A3 adds the sibling
fields to the same struct and validates them the same way.

**This entry measures the A2-only tree** (`0ecacd3` + A2), which is the branch
`feature/a2-capabilities-clean`. The phase was **finished** later on the shared branch
`feature/a2-capabilities`, which carries another agent's commits and therefore has
**different numbers** — the entry below has them.

---

### 2026-10-22 — Phase A2 finished: the host API reports what it changed, and the tree is measured honestly

**What I did.** The A2 work was already committed (`e537d70` and the fixes after it), so
this session did not build the gate again — it **finished** it. Three things were
missing or wrong, and all three are fixed:

1. **`audio_mute` did not return the previous mute state.** `DESIGN.md` §2 requires it:
   *"must return the previous mute state, and must not unmute a session the user
   muted"*. The implementation returned only counts (`2 session(s) muted, 1 left alone,
   0 could not be changed`), which says *how much* happened but not *what the state
   was*. It now names **every session of the process and the state it was in before the
   call** — `muted {session} (was playing)`, `unmuted {session} (was muted)`,
   `left alone {session} (was muted)`, `could not be changed {session} (was playing)` —
   and a process without an audio session gets `no audio session for pid {pid}`.
2. **The rule that a session the user muted is never unmuted again had no test.** The
   mute pass called the real session sweep, so nothing could be proved about it. The
   sweep and the ledger are now arguments of `mute_sessions`, and a unit test proves
   both halves: the user's own mute is left alone, the report says so, and the unmute
   puts back exactly what was changed.
3. **`window_hide` could report the visibility of a window that is not there any more.**
   A window handle outlives its window and Windows hands the same handle to another
   window later, so the recorded state was about a window that had gone. The handle is
   now checked with `IsWindow` before the old state is reported.

The self-test harness grew by **one** check: the capability pack now calls a **granted**
function (`read_file`) before the ungranted one, so the harness proves the gate is
**selective** — the pack's own function still ran, and only the other one was refused.
Its revert script uses granted functions only, so the revert cannot fail on a denial.

**Files touched.** `crates/armoury/src/killswitch.rs` (the mute pass, the report, two
tests), `crates/armoury/src/packs.rs` (the stale-handle check), `crates/armoury/src/script.rs`
(the mute reached end to end in a test), `tools/daemon-selftest.ps1` (one check, the
capability pack), `docs/modularity/DESIGN.md`, `DECISIONS.md` (§16), `GOTCHAS.md`
(traps 35–38), `README.md`, `PROGRESS.md`, and the gitignored plan file
`docs/plans/2026-10-21-modular-modules.md`.

**Measured.** All of it on the branch this phase is delivered on
(`feature/a2-capabilities`), whose parent is the art fetch through the Windows
certificate store.

| | before | after |
|---|---|---|
| The release exe | **12,857,344 bytes** (the tip before this session, `c104f32`) | **12,862,976 bytes** — **+5,632 (+0.04 %)** |
| The daemon's idle cost | **0.969 CPU s in 60 s = 1.615 % of one core** (median of 3) | **0.969 CPU s in 60 s = 1.615 % of one core** (median of 3) — **no regression** |
| The unit tests | 357 (266 in `armoury`, 91 in `armoury-core`) | **359** (268 / 91) — **+2** |
| The harness checks | 100 `Check` calls | **101** `Check` calls — **+1** |

*Idle cost:* the A1 method, unchanged. A throw-away instance — its own pipe, config
folder and single-instance mutex (`ARMOURY_TEST_NS`), started with `--autostart` so no
dashboard opens — with the three shipped packs **enabled** and **four tracked apps**,
which is the shape of the installed daemon. Twenty seconds of settling, then sixty
seconds of `TotalProcessorTime`. **The installed daemon was not stopped, started or
restarted**, and none of the four tracked apps exists on this machine, so no pack ran
and nothing on the machine moved. **Three samples of each binary, alternating, on the
same machine** — the single 60-second samples scatter over 0.812–1.016 CPU s, so one
sample proves nothing; the medians are identical.

*Exe size:* the release build of the tree, taken from the CI artefact.
`cargo build --release` is never run on this machine. **+5,632 bytes**, and the PE
section table says where: **+4,608 bytes of `.text`** (the mute pass, its report and
the `IsWindow` check) and **+1,024 bytes of `.rdata`** (the format strings). The
`.pdata` did not move.

*The phase as a whole on this tree:* the tree started at **12,796,416 bytes** (the art
fetch, which takes Mozilla's bundled roots **out** of the binary), so the whole of A2
costs **+66,560 bytes (+0.52 %)** here, against **+56,832 (+0.44 %)** on the A2-only
tree. **The baseline for the size rule on the delivered branch is 12,862,976 bytes.**

*The CI run:* **39731173747** (the workflow dispatch on the commit that carries the
phase's code), the job **green** in **6 min 22 s**: the debug build, **359 unit tests**
(268 + 91), and the self-test harness with **101 checks, all passed** — plus the
release build and the artefact the size was measured from. The branch's tip was
confirmed green afterwards, in **39733574798** (**6 min 41 s**), **39734338552**
(**6 min 34 s**) and **39735476837** (**5 min 29 s**): the same 359 tests and the same
101 checks every time. **The commits after those touch this folder only.**

**Decisions.** `DECISIONS.md` §16: `audio_mute` returns the previous mute state **per
session**, not per process and not as a count — a process can own several sessions and
they need not be in the same state, and one word for the whole process would either
lose that distinction or be a guess. §15 now carries the size of the delivered tree
next to the size of the A2-only tree, because a number without its tree is not a number.

**Surprises.**

- **The branch this work is delivered on is not the A2 tree.** It also carries the art
  fetch through the Windows certificate store, so it has **four more unit tests**,
  **five more harness checks** and a **68 KB smaller executable** than the A2-only
  copy. Every number in the folder therefore needs its tree, and the folder now says
  so (`GOTCHAS.md` trap 35).
- **The size baseline moved *down* without anybody touching the size rule.** The art
  fetch takes Mozilla's bundled roots out of the binary, so the "current size" went
  from 12,868,608 (A1) to 12,796,416. A baseline carried over from another branch
  would have been wrong in the *good* direction, which is the dangerous kind of wrong
  (`GOTCHAS.md` trap 36).
- **A single 60-second idle sample proves nothing.** The samples of one and the same
  binary scattered over 0.812–1.016 CPU s (1.35 %–1.69 % of a core). The A1 and A2
  entries each quote one sample; the numbers were close enough, but the honest way is
  a median of several, alternating between the two binaries.
- **`windows::Win32::UI::WindowsAndMessaging::IsWindow` takes an `Option<HWND>`,**
  while `IsWindowVisible` next to it takes a plain `HWND`. The first CI run of this
  session failed on exactly that one line (`packs.rs:1225`, `window_exists`) — and
  because it was the only error, it also proved that the rest of the change
  type-checked. The vendored source is in
  `%USERPROFILE%\.cargo\registry\src\…\windows-0.62.2`: read it, do not remember it.
- **A pack whose capability list does not include `journal` cannot call `ctx.log`** —
  and the harness's capability pack had exactly that in its revert script. It was
  harmless (the revert is skipped by its guard), but it was one guard away from a
  failed removal. The revert now uses a granted function only.
- **`HashMap::new` really is not a `const fn`** — the map is randomly seeded and the
  seed is the HashDoS protection, so it cannot be made at compile time. The folder's
  trap was right; it now says *why*, so that nobody "fixes" the ledger with
  `HashMap::with_hasher(BuildHasherDefault::new())`, which has a fixed hash
  (`GOTCHAS.md` trap 37).

**Next.** Phase A3 — `triggers` + `kind` in the pack schema (`crates/armoury-core/src/
pack.rs`). The pack gains a `kind` (`"pack"` or `"mode"`) and a `triggers` list, both
**additive**: the three shipped packs must keep loading, installing, applying, reverting
and removing unchanged, and a malformed `kind` has to be a named error like the other
schema errors. `WORK-ORDER.md` A3. The `Capability`/`CapSet` types and the additive
`capabilities` field are already in `armoury_core::pack` — A3 adds the sibling fields
to the same struct and validates them the same way, and it should reuse the same
`#[serde(default = ...)]` pattern the capability field uses.

---

## Template for new entries

```markdown
### <date> — Phase <X><n>: <what it was>

**What I did.** <the change, in one or two sentences>

**Files touched.** <paths>

**Measured.** <before → after, with the method. If a number did not move, say so.>

**Decisions.** <anything settled on the fly — and add it to DECISIONS.md too>

**Surprises.** <anything that contradicted the docs or the plan>

**Next.** <the next action, specific enough that you could start cold>
```
