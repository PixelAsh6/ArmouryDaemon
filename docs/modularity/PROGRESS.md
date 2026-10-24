# Progress

The running log. **Append to it at the end of every phase**, and update the block at
the top. Write it for someone with no memory of the session: **state, next step, and
anything surprising you found.**

---

## Current state

| | |
|---|---|
| **Phase** | A - **A1, A2, A3 and A4 done** (the TLS root store for the catalogue **and the cover art**, the capability-scoped host API, the pack schema's `kind` and `triggers`, the modes' own worker thread and channel). 1.1.6 is published and installed; A4 sits on the branch `feature/a4-mode-worker`, **not merged** |
| **Next action** | **A5** - the killswitch gets a config key and an IPC request (`WORK-ORDER.md` A5). The merge of A4 into `main` and the next release are the user's calls |
| **Branch** | `feature/a4-mode-worker` (cut from `main` `76877ba`) - CI **green** on the tip `26a58f9`: **383 unit tests** (278 in `armoury` + 105 in `armoury-core`), **146 harness checks** |
| **Tree** | clean |
| **Blocking** | nothing |
| **Open questions** | none - the design is settled in `DECISIONS.md`, and the review's one product question (`registry_set`'s capability) is settled in §20 |
| **Not measured** | the **idle cost** - no phase since 1.1.5 has touched the idle path, and the 1.1.5 tree's idle cost is itself not measured (the user declined the sampling). The number in `GOTCHAS.md` is the pre-1.1.5 one and does not belong to this tree |

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

### 2026-10-22 — the three branches integrated, and the re-timing released

**What I did.** The three green branches were integrated into one, `feature/phase-a`, and
the result is the **1.1.5** release.

| Branch | Tip | What it carried |
|---|---|---|
| `feature/a2-capabilities` | `ac404ec` | **Phase A2** — the capability-scoped host API |
| `fix/lockout-retime` | `494aa1b` | the cooldown re-timing, and the dashboard that names the app and says a change is waiting |
| `fix/art-fetch-windows-root-store` | `79ad7be` | the cover art through the Windows certificate store, `rustls-webpki-roots` dropped |

The branch was cut from A2 and the other two were merged into it. **Both merges were
clean**, and the reason is worth writing down: the art fetch was **already an ancestor**
of A2 (`2b119c6`), so that merge brought nothing new in content, and the harness's accept
fix is the same change twice (`aca3414` on A2, `79ad7be` on the art branch), which git
resolves on its own. The merged harness therefore carries **both** sets of checks — A2's
capability checks and the art branch's five root-store checks — and nothing was dropped.

The one line that was still missing: the snapshot's `cooldown_action` was hard-coded to
`None`, although the wire format has carried the field since the first release. The
daemon publishes it now, from the engine's own `pending_change`, so the dashboard's
cooldown row can say that a change is waiting behind the window — and the harness checks
that it does.

**Files touched.** `crates/armoury/src/daemon.rs` (the one line), `crates/armoury-core/
src/status.rs` (the field's doc comment), `tools/daemon-selftest.ps1` (the re-timing
section, seven checks), `Cargo.toml` and `Cargo.lock` (the version), `README.md`,
`CHANGELOG.md`, `release-notes/v1.1.5.md`, and this folder.

**Measured.**

| | before | after |
|---|---|---|
| The unit tests | 359 (268 in `armoury`, 91 in `armoury-core`) | **364** (270 / 94) — the lockout branch's five |
| The harness checks | 101 `Check` calls | **108** — **+7** (the re-timing section) |
| The release exe | 12,796,416 (the A2+art tree) | **12,865,024** — **+68,608 (+0.54 %)**; **+2,048 (+0.016 %)** over 12,862,976 (the delivered branch) |

**The release.** The integration was merged into `main` (`4712b44`, a normal merge, the
same tree as `feature/phase-a` — `0791c0e`) and the tag **`v1.1.5`** is on it. The
release is published: `https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.5`,
carrying **two assets and nothing else** — `ArmouryDaemon.exe` (12,865,024 bytes, sha256
`947ef3dc…24a4`) and its 85-byte sidecar. The published body is **2,270 bytes, byte for
byte** the committed `release-notes/v1.1.5.md`.

**The CI runs.**

| Run | What | Conclusion | Wall time |
|---|---|---|---|
| `39738969438` | CI on `feature/phase-a` (dispatch) | green — 364 tests, 108 harness checks, and the release profile | 8m42s |
| `39741535492` | CI on `main` (push) | green | 5m57s |
| `39742290289` | CI on the tag `v1.1.5` | green | 9m57s |
| `39742290367` | **Release** on `v1.1.5` | green — published and re-verified its own assets | 5m20s |

**Not measured.** The **idle cost** of this tree: it needs minutes of quiet sampling on
the user's machine, and the user was testing. The number in `GOTCHAS.md` is the
pre-1.1.5 one and does not belong to this tree.

**Decisions.** `DECISIONS.md` §17: a change of the cooldown **re-times the window that is
running** — the window keeps its start and takes the new length, so it ends at
`start + cooldown`, and the change it holds lands at the new expiry. The row **H21** of
the local-only `DECISIONS-2026-10-17.md` records the same decision, with the log excerpt
it is based on.

**Surprises.**

- **The merge that was supposed to conflict did not.** The folder's plan and the session
  notes expected the art branch's harness changes and A2's to collide. They do not: the
  art branch's work is **in** A2 (it is the ancestor `2b119c6`), and the one commit that
  was not — the accept fix — is byte-for-byte the same change as A2's `aca3414`. Git
  resolved it on its own, and the merged harness has both sets of checks. **A merge that
  resolves itself is not a merge that was checked**: the harness still had to be read to
  be sure that nothing was dropped.
- **The lockout branch touches no harness at all.** It changed `engine.rs` and `ui.rs`
  and nothing else, so the defect it fixed had **no harness check** — the harness set the
  cooldown only *before* each scenario, never while a window was running. That gap is
  the defect's real home, and the harness now closes it: a scenario that changes the
  cooldown while a window runs, and asserts that the window takes the new length and that
  the change it holds lands at the new expiry.
- **`cooldown_action` was `None` in the snapshot while the field was live in the wire
  format and in the dashboard's row.** The UI that reads it was written first and the
  daemon that fills it was not — one line in `App::status`, the last thing missing
  between the lockout model and what the user sees.

**Next.** Phase A3 — `triggers` + `kind` in the pack schema (`crates/armoury-core/src/
pack.rs`). The pack gains a `kind` (`"pack"` or `"mode"`) and a `triggers` list, both
**additive**: the three shipped packs must keep loading, installing, applying, reverting
and removing unchanged, and a malformed `kind` has to be a named error like the other
schema errors. `WORK-ORDER.md` A3. The `Capability`/`CapSet` types and the additive
`capabilities` field are already in `armoury_core::pack` — A3 adds the sibling fields to
the same struct and validates them the same way, with the same `#[serde(default = ...)]`
pattern.

---

### 2026-10-22 — Phase A3: the pack says what it is and what starts it

**What I did.** The pack schema gained two **additive** fields: `kind` (`"pack"` or
`"mode"`) and `triggers` (what starts the pack). A pack that carries neither loads and
behaves exactly as it did — the three shipped packs carry neither, and the catalogue in
`D:\ArmouryDaemon_Packs` needs no change. `spec` stays 1 or 2; there is no `spec: 3`. A
malformed `kind` is a **named error** — `unknown kind 'widget' (expected 'pack' or
'mode')` — refused when the pack is parsed: never a panic, never a silent default to
`"pack"`.

The **daemon does not act on the fields yet**, and that is what makes the phase additive:
the mode machinery — a worker of its own, a hotkey that dispatches to it — is A4 and A5.
The kind is in the log line that names a loaded pack, so an operator can see it.

**Files touched.** `crates/armoury-core/src/pack.rs` (the two fields, the two enums, the
trigger, the validation, six tests and four mutations of the hostile-pack battery),
`crates/armoury/src/packs.rs` (the loaded-pack log line), `tools/daemon-selftest.ps1`
(eleven checks), `CHANGELOG.md`, `compact.md` and this folder.

**Measured.**

| | before | after |
|---|---|---|
| The unit tests | 364 (270 in `armoury`, 94 in `armoury-core`) | **370** (270 / 100) — **+6**, green in CI |
| The harness checks | 108 `Check` calls | **119** — **+11**, green in CI |
| The release exe | 12,865,024 bytes (v1.1.5, the released tree) | **12,877,312 bytes** — **+12,288 (+0.10 %)**; the growth is code (+10,752 of `.text`) and the format strings (+1,024 of `.rdata`). `DECISIONS.md` §19 |
| The daemon's idle cost | not measured | **not measured** — the phase adds two fields to the schema and changes no runtime path, and the idle cost of the 1.1.5 tree is itself not measured (the user declined the sampling) |

*The exe size:* the release build of the tree, taken from the CI artefact of the dispatch
(`ArmouryDaemon.exe`, sha256 `cef00526…9ba`), measured against the **published v1.1.5
asset** (12,865,024 bytes, sha256 `947ef3dc…24a4` — the copy installed on the machine).
`cargo build --release` is never run on this machine.

*The CI runs:*

| Run | What | Conclusion | Wall time |
|---|---|---|---|
| `39777594088` | CI on the code commit `0f0b104` (dispatch) | green — 370 tests, 118 harness checks, and the release profile | 9m54s |
| `39780061907` | CI on the folder commit `55aed31` (dispatch) | green — the same 370 tests and 118 checks | 9m40s |
| `39781519178` | CI on the branch's tip `8341452` (dispatch) | green — **370 tests, 119 harness checks**, and the release profile | 10m40s |
| `39783613729` | CI on the folder commit `bb2f234` (dispatch) | green — the same 370 tests and 119 checks; the artefact is the same 12,877,312 bytes | 10m45s |

**Decisions.** `DECISIONS.md` §18: the defaults (`"pack"`, and the tracked app for every
kind), the named error, the two trigger rules the folder did not state (a `hotkey` trigger
needs a key, a `tracked_app` trigger takes none), the key name not being checked against
the daemon's hotkey table, and the daemon not acting on the fields yet. §19: the size.
`DESIGN.md` §1 carries the shape of a trigger.

**Surprises.**

- **serde drops the fields the schema does not know.** A pack that carried
  `"kind": "mode"` loaded as an ordinary pack before this phase — the field was read and
  thrown away. So the daemon's behaviour could not change, and the phase could not break
  the shipped packs: the schema only learned to say what it always meant
  (`GOTCHAS.md` trap 41).
- **A custom serde error has no position of its own, and serde_json gives it one.**
  `Error::custom` builds it at line 0, column 0, but serde_json's `fix_position` replaces
  that with the position it was reading — so the named error reads *"invalid JSON: the
  field 'kind': unknown kind 'widget' (expected 'pack' or 'mode')"*, with the field named
  by the walk back over the text (`GOTCHAS.md` trap 42).
- **The folder's `triggers` default is one default for every kind.** A mode that does
  not say what starts it inherits the tracked app; the rule "a mode needs at least one
  trigger" therefore bites on an **explicitly empty** list, not on an absent one. A
  default that depended on the kind would need the schema to tell an absent field from
  an empty one — `serde(default = …)` cannot see the other fields.
- **The packs-repo README does not document `capabilities` either.** The pack format is
  documented in `D:\ArmouryDaemon_Packs\README.md`, and A2's `capabilities` field is not
  in it. The **catalogue** needs no change for A3 — the fields are additive — but the
  format doc is behind. It was not touched here: it is another repository, and the
  phase before this one did not touch it either.
- **The harness count is a static scan, and the scan has to be anchored** — `^[ \t]*Check
  [ \t]`, or the number counts calls that are not calls (`GOTCHAS.md` trap 31).

**Next.** Phase A4 — a separate worker queue for modes. The pack worker is a single
serial thread (`packs.rs:168`), so a slow mode must never block a pack apply/revert:
modes get their own thread and their own channel. `WORK-ORDER.md` A4. The daemon-side
half of what A3 put in the schema — a trigger that starts something, a mode the hotkey
dispatches to — comes with it.

---

### 2026-10-23 - A3 merged, and the review of 1.1.5 closed

**What I did.** Two jobs, in this order.

**A3 is on `main`.** `feature/a3-kind-triggers` (tip `fbb1ef9`, cut from `79ad905`) was
merged with a normal, non-forced merge into `21935ff` and pushed; the `main` CI run
**`39803275300`** is green after **3m53s**. The tree it lands is the one A3's own runs
proved: 370 unit tests and 119 harness checks.

**The review of 1.1.5 found five defects, and all five were real.** The work is on
**`fix/1.1.5-review`**, cut from the merge, and is **not merged** - that is the user's
call. Each one was verified against the code before it was fixed:

- **The capability gate did not cover the declarative steps** (the severe one). The script
  is gated structurally - only the granted functions are on its engine - and the **steps**
  were not gated at all, because a step is data and no engine ever sees it. A pack that
  declared `["files"]`, or `[]`, could still run programs through `cli`, send keystrokes,
  kill processes and write the registry. Every mechanism now names a capability
  (`Capability::of_mechanism`), `Pack::validate` refuses the pack **when it is parsed** so
  it never loads, and `run_step` refuses it again where it runs.
- **`set_cooldown` resurrected a window that had ended.** The engine keeps an expired
  window until a tick reaps it, and the daemon drains the pipe **before** the 1 Hz
  housekeeping that ticks - the race the review described. `set_cooldown` takes `now` and
  drops a window whose `end` is past.
- **The overlay request queue was unbounded.** One pending request now, and the daemon
  skips a request for the state it is already in.
- **The denial was lost inside a script-defined function.** Rhai wraps it in
  `ErrorInFunctionCall`; `denial()` reads `unwrap_inner()` now.
- **`audio_mute_all` left no crash-recovery record.** Its ids go into `idle.ledger`, the
  file idle mode already uses, so `recover_if_needed` unmutes them on the next start.

Also from the same review: the startup path **clamps `cooldown_seconds` to 5-3600 s**, so
a hand-edited config cannot panic the daemon in `Window::end()`.

**Files touched.** `crates/armoury-core/src/pack.rs` (the mechanism table, the `registry`
capability, the step gate in `validate`, five tests), `crates/armoury-core/src/engine.rs`
(`set_cooldown` takes `now`, one test), `crates/armoury/src/packs.rs` (`run_block`/`run_step`
take the `CapSet`, one test; the overlay request is one `Option`), `crates/armoury/src/
daemon.rs` (the drain, `set_cooldown`, the clamp), `crates/armoury/src/script.rs`
(`unwrap_inner`, one test), `crates/armoury/src/overlay.rs` (`ledger_add`/`ledger_remove`,
one test), `tools/daemon-selftest.ps1` (fourteen checks), `CHANGELOG.md`, `compact.md` and
this folder.

**Measured.**

| | before | after |
|---|---|---|
| The unit tests | 370 (270 in `armoury`, 100 in `armoury-core`) | **378 (273 in `armoury`, 105 in `armoury-core`)** - **+8** |
| The harness checks | 119 `Check` calls | **133** - **+14** |
| The release exe | 12,877,312 bytes (the A3 tree) | **12,879,360 bytes** - **+2,048 (+0.016 %)**, sha256 `611dcb98…b2ab` |

*The exe size:* the release build of the branch, taken from the CI artefact of the
dispatch (`ArmouryDaemon.exe`, sha256 `611dcb98…b2ab`), measured against the A3 tree's
12,877,312. No size-bearing decision is made here - the fix is small and the rule is not
bent (`DECISIONS.md` §12 and §19 bend it; this one does not).

*The CI runs:*

| Run | What | Conclusion | Wall time |
|---|---|---|---|
| `39806787130` | CI on the fix branch (dispatch) - **red**: `'go' is a reserved keyword` in the new denial test | failed | 1m26s |
| `39807213693` | CI on the code commit `41ba47e` (dispatch) | **green** - 378 tests, 133 harness checks, and the release profile | 8m49s |
| `39809146891` | CI on the branch's tip `ada6f9c` (dispatch) - the same tree, with the folder on it | **green** - the same 378 tests and 133 checks; the artefact is the 12,879,360 bytes above | 7m46s |
| `39810878555` | CI on `d750efe` - **cancelled by the next dispatch** (`cancel-in-progress` on the branch's concurrency group), not a failure | cancelled | 1m24s |
| `39811080550` | CI on the branch's tip `18a7c28` (dispatch) - the anchors re-read, docs only | **green** - 378 tests, 133 checks, `ALL CHECKS PASSED` | 10m24s |

**Decisions.** `DECISIONS.md` **20**: the gate covers the declarative steps, and
`registry_set` gets a capability of its own (`registry`) instead of being folded into
`files-write`. **21**: **the game wins** - a start inside the window cancels the pending
revert, the machine stays in Turbo and the window keeps running. That is the user's
decision of 2026-10-23, the code already behaved that way (H20, rule (4)), and nothing in
the engine was changed for it: it is pinned by a harness check (9b) and recorded.
**17** gains the amendment for the window that has ended.

**Surprises.**

- **The gate was structural in exactly one place and absent in the other.** The whole
  point of §14's "the denial is structural, not a check a call site could forget" is that
  the *engine* is the gate - and a declarative step never reaches an engine. The question
  that finds this class of hole: **what are the other paths to the same effect?**
  (`GOTCHAS.md` trap 43).
- **The daemon's loop order is a security-relevant detail.** Pipe, watchers and the other
  channels are drained at the top of the iteration; housekeeping - and so `engine.tick()` -
  runs after them. Anything the loop reads before that tick must be safe on a state the
  tick has not reached yet: the engine's accessors already took `now`, the *mutator* did
  not (`GOTCHAS.md` trap 44).
- **`go` is a reserved keyword in Rhai 1.26.1.** The first CI run of the branch was red on
  a script that never parsed - `'go' is a reserved keyword (line 1, position 4)` - so the
  test failed before it could fail for the reason it was written. The function is
  `send_bang` now. A Rhai `fn` also does **not** capture the caller's scope: the context
  has to be passed in as a parameter (`GOTCHAS.md` trap 45).
- **`idle.ledger` now has two writers**, and it is a read-modify-write. Writing it whole
  from either side loses the other side's record - and a lost record is a machine left
  muted after a crash - so it is written through `ledger_add`/`ledger_remove` under one
  lock (`GOTCHAS.md` trap 46).
- **The harness can only catch the phantom lockout sometimes.** The gap it lives in is up
  to one second wide (the 1 Hz tick), and the fixed build passes whichever way the race
  goes: check 12c fails only on a build that has the defect and only when it lands in the
  gap. That is the right direction for a flake - it can be a false *pass* on the old code,
  never a false failure on the new one.

**Next.** A4 - a separate worker queue for modes (`WORK-ORDER.md` A4). The merge of
`fix/1.1.5-review` into `main` and the next release are the user's calls.

---

### 2026-10-22 - 1.1.6: the review's fixes merged, released and installed

**What I did.** The review of 1.1.5 was merged into `main` as `95b0797` - its CI,
`39816227849`, is green after 7m24s - and the tree became **1.1.6**: the workspace
version moved, the release notes and the changelog section were written, the tag
`v1.1.6` was cut on `a61740f`, the Release workflow published, and the published
binary was installed on this machine.

**Files touched.** `Cargo.toml` (1.1.5 -> 1.1.6), `Cargo.lock` (the two workspace
packages, by `cargo fetch`), `tools/daemon-selftest.ps1` (the `--version` check names
the version), `README.md` (the badge, the version in the examples, the release link,
and the test counts, which were stale at 364/108), `deploy/README.md`,
`release-notes/v1.1.6.md`, `CHANGELOG.md`, this folder, `compact.md`.

**Measured.**

| | before | after |
|---|---|---|
| The release exe | 12,865,024 bytes (v1.1.5, the published asset) | **12,879,360 bytes** - **+14,336 (+0.11 %)**, sha256 `0bc14524.56d4` |
| The branch tree's artefact | 12,879,360 bytes (the fix branch's CI artefact) | **12,879,360 bytes** - the release build reproduces the size exactly |
| The unit tests | 378 | **378** - the fixes and the version carry no new test |
| The harness checks | 133 | **133** |

*The size:* the release is **+2,048 (+0.016 %)** over the A3 tree's 12,877,312 and
**+14,336 (+0.11 %)** over the v1.1.5 asset. No size-bearing decision - the growth is
the review's fixes, and `DECISIONS.md` §19 already records the schema phase's bend.

*The runs:*

| Run | What | Conclusion | Wall time |
|---|---|---|---|
| `39816227849` | CI on `main` at `95b0797` (the merge of the review) | green - 378 tests, 133 harness checks | 7m24s |
| `39818292103` | CI on `main` at `a61740f` (the version, the notes, the changelog) | green - 378 tests, 133 checks, `ALL CHECKS PASSED` | 6m53s |
| `39819468547` | CI on the tag `v1.1.6` | green - 378 tests, 133 checks | 9m58s |
| `39819468439` | **Release** on `v1.1.6` | green - built, wrote the sidecar, published, re-downloaded and verified its own assets | 7m27s |
| `39822071450` | CI on `main` at `aeb1c74` (the folder and the README, docs only) | green - 378 tests, 133 checks | 4m08s |
| `39822911473` | CI on `main` at `04a6f71` (the entry names its own run, docs only) | green - 378 tests, 133 checks | 4m07s |

**The published release.** `https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.6`,
published 2026-10-22 17:11:47Z, two assets and nothing else:

- `ArmouryDaemon.exe` - **12,879,360 bytes**, sha256
  `0bc14524aa20c3f424ef8aa224845ec13f34d0115222fe3e6affe784904556d4`
- `ArmouryDaemon.exe.sha256` - **85 bytes**, sha256
  `7a65af45fd38d1b7699dafeb5aee2ab26103b32a681af6cad0bdab472b6165c0`; the hash
  field in it is the exe's.

Downloaded back and checked: the exe's hash is the API digest and the sidecar's field,
the sidecar is exactly 85 bytes, and the body is **2,987 bytes**, byte for byte the
committed `release-notes/v1.1.6.md`.

**The install.** The backup `C:\Apps\ArmouryDaemon_backup_2026-10-22-2005` (8 files,
12,907,474 bytes - the same totals as the live folder) was made first and **deleted**
once the install verified. The daemon was asked to quit over `\\.\armoury-daemon.pipe`
and left in **1.292 s**; nothing was killed. The published exe was copied in and
`setup.ps1` ran elevated (exit 0). Verified: the task `ArmouryDaemon` is **Running**,
last result **267009**, action `C:\Apps\ArmouryDaemon\ArmouryDaemon.exe --autostart`,
`RunLevel Highest`, and its description carries `0bc14524...56d4`; the sidecar is 85
bytes and matches; the folder's ACL is applied with `AreAccessRulesProtected = True`
and no inherited entries; the daemon's token is **High (0x3000)**; and
`%APPDATA%\ArmouryDaemon\logs\daemon.log` reads `daemon starting (v1.1.6)` then
`daemon ready`.

**Decisions.** None new. The two the release ships - §20 (the gate over the
declarative steps and the `registry` capability) and §21 (the game wins) - were settled
in the review session and are recorded there.

**Surprises.**

- **The version lives in the harness as well.** `tools/daemon-selftest.ps1` checks
  `--version` against the literal `ArmouryDaemon 1.1.5`, and the CI runner is elevated,
  so that check really runs: a version bump that skips that line turns `main`'s CI red
  on the first push. The bump touches `Cargo.toml`, `Cargo.lock` and the harness, and
  the README's badge, examples and release link, or the tree contradicts itself.
- **The release build reproduces the branch artefact's size but not its bytes.**
  Both are 12,879,360, and their hashes differ (`611dcb98` on the branch, `0bc14524`
  on the release) - the link stamps a PE timestamp and a per-link constant, as
  `GOTCHAS.md` says. The size is a property of the source; the bytes are not.

**Next.** A4 - a separate worker queue for modes (`WORK-ORDER.md` A4). 1.1.6 is out
and installed; the next release is the user's call.

---

### 2026-10-23 — Phase A4: the modes have a worker of their own

**What I did.** The pack worker stopped being a single serial thread. `packs::start`
(`packs.rs:161`) now takes **two** job receivers and spawns **two** workers — the plain
packs keep their serial thread with the old shape, the modes get a second one
(`spawn_worker`, `packs.rs:186`, the extracted loop) — so a mode that takes five
seconds applies and reverts on its own thread while a plain pack's apply or revert
goes through untouched. The daemon decides which half a job belongs to by reading the
catalogue: a whole-state job is split by `pack.kind` (`split_enabled`,
`daemon.rs:829` — an unknown id stays on the pack side, which is the pre-A4
behaviour), a tagged single-pack job follows the target pack's kind (`worker_for`,
`daemon.rs:1534`), and a mode half that carries nothing is skipped rather than sent
empty (`send_by_kind`, `daemon.rs:1513`).

The two halves report on **one** outcome channel as before, and the outcome now
carries `scoped` — the echo of the enabled ids **its own** half carried
(`packs.rs:70`) — so `on_pack_outcome` can merge a half order-independently of the
other (`merge_scoped`, `daemon.rs:850`). The running state, the journal and the wire
format did not change, and `pack.rs` is untouched — the schema phase (A3) already put
the `kind` in, and A4 is the first phase that acts on it.

A mode job still arrives only through the ordinary trigger path (`tracked_app` riding
a whole-state job): A4 is the worker split, **not** the dispatch. `triggers` is read
by nobody yet — A3 put it in the schema and no phase acts on it — and the folder does
not schedule the dispatch: the next phase (A5) is the killswitch's config key and IPC
request, so a `hotkey` trigger stays inert until a phase says otherwise.

**Files touched.** `crates/armoury/src/packs.rs` (the two-worker `start`, the
extracted `spawn_worker`, `PackOutcome::scoped` and its fill in `run_job` at `packs.rs:410`,
two tests), `crates/armoury/src/daemon.rs` (the `kind` on `PackMeta` at `:804`, the
`mode_tx` channel and the kind helpers, the split sends in the four places that emit
jobs — the startup journal revert, `send_transition`, `apply_now_if_triggered`,
`revert_before_quit` — and the merge in `on_pack_outcome`, three tests),
`tools/daemon-selftest.ps1` (the timing helper `Get-PackLineTimes` at `:207` and the
scenario **19d**, two modes either side of the plain pack), `CHANGELOG.md`,
`compact.md` and this folder.

**Measured.**

| | before | after |
|---|---|---|
| The unit tests | 378 (273 in `armoury`, 105 in `armoury-core`) | **383** (278 / 105) — **+5**, green in CI |
| The harness checks | 133 `Check` calls | **146** — **+13**, green in CI |
| The release exe | 12,879,360 bytes (v1.1.6, the installed tree) | **12,895,744 bytes** — **+16,384 (+0.13 %)**, sha256 `b504af1c…8a4f` |
| The daemon's idle cost | not measured | **not measured** — the user declined the sampling; the number in `GOTCHAS.md` is the pre-1.1.5 one and does not belong to any tree since |

*The exe size:* the release build of the branch, taken from the CI artefact of the
tip's dispatch (`ArmouryDaemon.exe`, 12,895,744 bytes, sha256
`b504af1c4a26df76ace652be024d9c8868d4f232f0ef240e477b20d9a6958a4f`, and the 85-byte
sidecar beside it), measured against the published v1.1.6 asset (12,879,360 bytes,
sha256 `0bc14524…56d4` — the copy installed on the machine). The same tree linked
twice differs in its bytes: the earlier dispatch of the same Rust code produced
12,895,744 bytes hashing `864e6064…987c`, so the size is the fact and the hash
identifies one link. `cargo build --release` is never run on this machine.

*The acceptance, in the folder's own words:* **a mode that takes 5 seconds does not
delay a pack revert** (`WORK-ORDER.md` A4). The harness proves the whole sentence
with real timing, from log timestamps (19d): **two** modes whose steps sleep 5 s — one
in its apply, one in its revert — and a plain pack between them in `order`, all three
enabled, `timeout.exe` tracked. `order` is what makes each direction testable: an
apply walks the packs ascending and a revert descending, so the mode slow in its
*apply* (order 9) stands ahead of the plain pack (order 10) in an apply and the mode
slow in its *revert* (order 11) stands ahead of it in a revert. On the single serial
worker A4 replaced, one of the two was always ahead of the pack and the pack's own log
line would have followed five seconds later. As it is: the fast pack's apply line
lands **≤ 3 s** after the send line and the slow mode's **≥ 3.5 s**, and after the
tracked app is killed the fast pack's revert is again **≤ 3 s** while the mode that
reverts first takes **≥ 3.5 s**. The log's timestamp resolution is ~1 s, so the
margins are wide by design, and every check fails with the numbers behind it.

*The CI runs:*

| Run | What | Conclusion | Wall time |
|---|---|---|---|
| `39965300253` | CI on the code commit `a6aa1a9` (dispatch) - **red**: `E0593`, the journal merge's `retain` closure took one argument where `HashMap` wants two | failed | 1m48s |
| `39965693668` | CI on `6fe01ce` (dispatch) - **red**: the slow-mode harness test applied **nothing** (`applied_ids []` against `["fastpack-test"]`) — the test packs' guardless blocks skip while the target is away | failed | 2m04s |
| `39966344387` | CI on `9e4fd63` (dispatch) | **green** - 381 tests, 144 harness checks, `ALL CHECKS PASSED`, and the release profile: the 12,895,744-byte exe above | 8m09s |
| `40121307649` | CI on the branch's tip `26a58f9` (dispatch) — the revert half of 19d now has a mode of its own to fail against | **green** - **383 tests, 146 harness checks**, `ALL CHECKS PASSED` | 8m22s |

**Decisions.** `DECISIONS.md` **22**: the split is by `kind` read from the catalogue,
the two workers share **one** outcome channel with the echo merge, and a tagged job
follows the target pack's kind. Rejected there: a priority lane on still-one thread,
and per-worker outcome channels with a split drain.

**Surprises.**

- **`HashMap::retain` takes a two-argument closure** (`|k, v|`). The journal merge
  was written with a one-argument predicate and did not compile — `E0593` on the
  first CI run. Small, but it is the cost of never compiling locally spelled out:
  `cargo check` would have said it in seconds, CI said it in 1m48s
  (`GOTCHAS.md` trap 47).
- **A `guard` you do not write is a guard that says "only while the target runs".**
  `Guard::default()` defaults to `target_running: true` (`pack.rs:521`), so a
  `StepBlock` without a `guard` **skips entirely** while the tracked app is closed.
  That is what made the slow-mode test apply nothing at all and turned the second CI
  run red. Both the unit-test packs and the harness packs now say
  `"guard": { "targetRunning": false }` in the apply **and** the revert blocks — and
  the shipped packs carry explicit revert guards for the same silent reason
  (`GOTCHAS.md` trap 48).
- **The revert half of the new check could not fail.** 19d passed on the first green
  run, and the revert-side checks passed *for the wrong reason*: a revert walks the
  packs in descending `order`, so the plain pack (order 10) already ran before the
  slow mode (order 9) on the old single serial worker — the check watched a delay that
  the defect could not have produced. One mode cannot be both first in an ascending
  apply and first in a descending revert, so the scenario has two: the mode slow in
  its apply sorts below the pack, the one slow in its revert sorts above it, and each
  timing check now has a mode ahead of the pack to fail against
  (`GOTCHAS.md` trap 50).
- **In PowerShell, `@($x)[-1]` on an empty array is `$null`, not an error.** The
  timing helper turns a line it never found into a gap of `9999`, so a missing log
  line fails as a number in the assertion instead of as a parse error in the
  plumbing — the right direction for a harness failure.

**Next.** Phase A5 — the killswitch gets a config key and an IPC request
(`WORK-ORDER.md` A5): it has neither today, and the state has to survive a restart;
that is also the natural place for the "is the killswitch module installed" flag.
The merge of `feature/a4-mode-worker` into `main` and the next release are the
user's calls.

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
