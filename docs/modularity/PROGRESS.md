# Progress

The running log. **Append to it at the end of every phase**, and update the block at
the top. Write it for someone with no memory of the session: **state, next step, and
anything surprising you found.**

---

## Current state

| | |
|---|---|
| **Phase** | A — **A1 done** (the TLS root store), A2 next |
| **Next action** | **A2** — the capability-scoped host API (`WORK-ORDER.md` A2) |
| **Branch** | `main` (the A1 commit is on top of `9276196`, version **1.1.4**, released) |
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
