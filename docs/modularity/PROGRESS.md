# Progress

The running log. **Append to it at the end of every phase**, and update the block at
the top. Write it for someone with no memory of the session: **state, next step, and
anything surprising you found.**

---

## Current state

| | |
|---|---|
| **Phase** | A — not started |
| **Next action** | **A1** — fix the `ureq` TLS root store (`WORK-ORDER.md` A1) |
| **Branch** | `main` (the 1.1.4 work is committed on top of `0350aac`) |
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
