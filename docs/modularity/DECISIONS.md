# Decisions

What is settled for the modular-modules work, and what was rejected and why. **Do not
re-litigate a settled decision without a reason.** If one turns out to be wrong, change
it here in the same commit and say why.

---

## 1. The module system extends the existing pack + catalog machinery

**Settled.** Three module kinds, one signed catalog, one "Modules" window, one install
path.

| Kind | What it is | Items |
|---|---|---|
| `pack` | Data + optional sandboxed script. Zero compiled-in per-module code. | G-Helper, Rainmeter, Wallpaper Engine |
| `mode` | A pack with a *global trigger* instead of a tracked-app trigger. | Killswitch, Idle |
| `helper` | A signed executable the daemon spawns. | Console launcher |

**Why:** ~80 % of the plumbing already exists and is proven in production on this
machine — download, SHA-256 + size verify, `min_engine` gating, install, remove,
update, and the "Install packs" window. A new plugin framework would throw all of
that away and re-earn every lesson.

**Rejected: a new plugin framework (a registry, a trait, a loader).** The pack model
already *is* the module model. The only thing missing is a `kind` and a `triggers`
field.

---

## 2. Modes need no new engine state

**Settled.** `Engine::set_idle` (`crates/armoury-core/src/engine.rs:183-207`) is
already a first-class mode — its own doc comment says *"A mode change is deliberate: it
drops a running lockout and starts none."* Entering idle reverts the applied packs;
leaving re-applies when a tracked app is running.

So the two mode behaviours map onto machinery that already exists:

| Mode flag | Meaning | Drives (already there) |
|---|---|---|
| `suspends_packs: true` | while the mode is on, the tracked-app engine is frozen | `Engine::set_idle(true/false)` — `engine.rs:183` |
| `react_to_tracked: true` | a tracked app that starts while the mode is engaged is handled too | the re-engage at `crates/armoury/src/daemon.rs:1334-1339` |

**Why:** the daemon drives the existing transitions; the engine is not touched.

**Rejected: adding mode state to the engine.** It would duplicate `set_idle` and split
the lockout semantics across two places.

---

## 3. The Rust stays where it is; the *capability* is gated

**Settled.** The killswitch's `Ledger` (`crates/armoury/src/killswitch.rs:71`) and the
overlay's crash-recovery ledger (`crates/armoury/src/overlay.rs:554-562`, `:638`) are
subtle, exact-restore code. **Making a feature modular does not require moving its
implementation out of the binary** — gating the capability on an installed module is
enough, and it satisfies the user's UX.

**Why:** moving working code out risks correctness for no user-visible gain, and the
user's ask is "the user chooses what to download", not "the code must live elsewhere".

**Rejected: moving the killswitch and idle into WASM components now.** That is Phase E
and it is deferred. It is worth it only for third-party authors.

---

## 4. Trust: minisign over the catalog manifest

**Settled.** `minisign-verify` 0.3.0 (zero dependencies, Ed25519) over the manifest
bytes. The trust anchor is a **keyring baked into the exe** with `include_str!`, two
keys live at a time with a 90-day overlap.

**Why:** one signature transitively authenticates every payload, because each manifest
entry already carries SHA-256 and size. The channel is first-party, so key rotation
via a keyring is enough.

**Rejected:**
- **TUF.** The `tuf` crate is beta ("the API is unstable and you should be prepared to
  refactor on even patch releases") and `tough` does not support delegated roles
  (TAP 3/4). Revisit if third-party publishers ever appear.
- **Sigstore / cosign.** It replaces minisign rather than sitting beside it, and it
  buys a non-repudiable audit trail a first-party channel does not need.
- **TLS certificate pinning.** It fights corporate TLS interception — exactly the case
  where a signed manifest still works. `rustls` has no pinning API, and a `dangerous`
  verifier disables chain, expiry **and** hostname checks.
- **An HKCU trust anchor.** A user-editable anchor is a user-owned key.

---

## 5. The build targets x86-64 v3, not `native`

**Settled — already implemented in 1.1.3.** `.cargo/config.toml` sets
`-C target-cpu=x86-64-v3` (AVX2, BMI1, BMI2, F16C, FMA, LZCNT, MOVBE, XSAVE). The same
binary on every machine; runs on Intel Haswell (2013) and AMD Excavator (2015) and
later.

**Why:** it is what lets the CI runner produce the published artefact, and it is
portable across ~12 years of hardware.

**Rejected: `target-cpu=native`.** Not portable. It was the old local-build setting and
the project deliberately moved away from it.

---

## 6. The console becomes a helper EXE

**Settled.** A new bin target; the daemon spawns the installed helper instead of
`current_exe() --console`.

**Why:** the console genuinely cannot be a pack. It is a GUI process with XInput
polling and egui animation; a pack is a data file the daemon interprets.

**Known trade-off, accepted:** this saves little *disk* — the core still needs eframe
for the dashboard. Its value is independent updates and removal, not bytes.

---

## 7. Inno Setup for the core installer, shipping zero packs

**Settled.** Inno Setup 6.7.3, `PrivilegesRequired=lowest`,
`PrivilegesRequiredOverridesAllowed=dialog`, `/VERYSILENT /SUPPRESSMSGBOXES
/NORESTART`. The installer ships the **core only**; the catalog is the only source of
modules.

**Why:** it is the VS Code model (the installer ships the core, the marketplace ships
the extensions) and it makes the project winget-installable.

**Rejected:**
- **MSIX.** Disqualified: it forbids per-user Windows services, blocks HKLM writes,
  forbids elevation, and states that "your app's modules are loaded in-process to
  processes that are not in your Windows app package. This isn't permitted."
- **MSI.** Its feature lists are frozen at build time — "MSI is just not built for
  downloading additional data and running arbitrary application logic".
- **Squirrel.Windows.** Unmaintained; its README still carries a "Contributors
  Needed" banner.

---

## 8. The build-from-source script is the auditor channel

**Settled.** A script that clones the source at a pinned tag, verifies it, builds with
the same `x86-64-v3` target, stages the exe, then deletes `target/` and the source.

**Why:** its value is **trust and customization**, not CPU features. "I built this from
the source I can read" fits a product that already ships data-only packs, a sandboxed
script host and no telemetry.

**Rejected:**
- **"Compile native for the user's CPU" as the selling point.** The payoff is ~0–5 %
  and unmeasurable — the workload is a 1 Hz process walk, some Win32 calls, JSON
  parsing, and a GPU-bound UI (35 µs of CPU per frame in the `ui` callback). You would
  charge the user a Rust toolchain install and 15 minutes of build time for a number
  they cannot see.
- **Asking which packs to install at build time.** The packs are plain JSON in a
  separate repo; bundling them means rebuilding the exe to change a pack. The runtime
  catalog already does it better.

---

## 9. WASM is deferred and additive

**Settled.** Rhai stays. A WASM component is the right answer only if third-party pack
authors matter, and then only for modules that need real isolation.

**Why:** the measured cost is real — `wasmtime` adds +5.98 MB over baseline (reduced
features +4.01 MB) to a binary that already carries 3 MB of Rhai. `wasmi` is +1.11 MB
but has **no Component Model**, so no WIT. `Extism` 1.41 is +8.52 MB and pins a stale
wasmtime 49, so you would carry two wasmtime copies.

**The prototype, if it is ever wanted:** the killswitch as a WIT component. A plugin
that imports an interface you did not wire **fails to instantiate** — the denial is
structural, which is the property worth buying.

---

## 10. The lockout holds a change in both directions

**Settled — implemented in 1.1.4.** The lockout is a window in which **no change
may touch the packs**, in either direction. A tracked app that starts when **no
window is running** applies at once and starts a window of its own; a tracked app
that starts **inside** a window is **held** and lands at the expiry — if the app is
still running by then — **exactly once**; a **close is always held** (it starts a
window if none is running and keeps the running one rather than restarting it); a
**restart inside the window cancels the hold**; a **manual action** (pause,
removing a tracked app, entering or leaving idle mode) **bypasses** it. The code
is `crates/armoury-core/src/engine.rs`: `try_change` (`:314`), `tick` (`:291`),
`lock_running` (`:366`).

**Why:** the model has to be **symmetric**, or the machine changes in the middle of
a cooldown. The 21 Oct flip-flop had two causes and the old model fixed only one
of them: a close that landed **between** two windows put the machine back at
once, and the restart that followed then waited out the revert's own window — so
the machine sat in the quiet mode while the game ran, and the logs showed the
revert before the apply. A **held** change also lands **exactly once**, and only
if it is still wanted: a change that is taken back before the expiry never lands.

**Rejected: "the apply is never deferred, the lockout holds the revert" (the
2026-10-21 model).** It was an **over-correction**: the 21 Oct flip-flop was fixed
by holding the close, and never needed the apply to be exempt from the window. It
also made the model asymmetric, so a start inside a window changed the machine in
the middle of a cooldown — which the user rejected on 2026-10-22.

**Also rejected: "the lockout is a minimum spacing between two changes"** — the
model before 2026-10-21. A change that does not happen starts no window, and a
window that expires while the intent still holds changes nothing.

---

## 11. The catalogue client is one agent, built once, with the platform verifier

**Settled — implemented in A1.** `crates/armoury/src/catalog.rs` builds **one** `ureq::Agent`
from a `Config` that carries a `TlsConfig` with `RootCerts::PlatformVerifier`, and every
fetch goes through that agent. The client is built on the **first** fetch, not at startup.

**Why:**

- The **Windows certificate store** is the root store. The machine's own store carries the
  enterprise roots, the constraints and the revocation policy, and the catalogue has to be
  reachable on a machine with a corporate CA or with TLS interception. Mozilla's bundled roots
  know none of them.
- **One agent, not one per request.** A request-level TLS change **bypasses the agent's
  connection pool** — ureq says so itself. More important: the rustls `ClientConfig` and the
  platform verifier are built **once per agent**, not once per fetch, and the verifier opens
  the certificate store.
- **The client is built on the first fetch.** The daemon's idle path never touches the
  network, and nothing may be added to it.
- The **WinINet** proxy settings, not WinHTTP: this is a per-user desktop app. WinHTTP is for
  a Windows *service*.

**Known trade-off, accepted:** the proxy settings are read when the client is built, so a
proxy change is only picked up after the daemon restarts. Reading them per fetch would mean a
new client per fetch — the cost this decision removes.

**Rejected:**

- **`ureq::get(url).config()…build()`** — the request-level config the folder's first draft
  showed. It bypasses the pool, and it builds a new client for every request.
- **One agent per request** (what the code did before): a new rustls config and a new
  platform verifier for every fetch, and no connection reuse.
- **Mozilla's roots** (`RootCerts::WebPki`, ureq's default): the bug this phase fixes.
- **Pinning the catalogue's certificate**: it fights the TLS interception this phase is about,
  and `rustls` has no pinning API.

---

## 12. The catalogue client may add 60 KB — the size rule bends here, once

**Settled — implemented in A1.** The release build grows from **12,808,192 bytes** (v1.1.4)
to **12,868,608 bytes**: **+60,416 bytes, +0.47 %**. `WORK-ORDER.md` says only Phase E may
add bytes. This phase adds them anyway, and the reason is in the numbers.

**Why:** a catalogue that cannot be fetched is worth more than 60 KB of a 12.8 MB executable.
The rule exists to catch *gratuitous* growth — a module system, a WASM runtime — and not to
freeze a bug fix that makes the product work on the machines it is sold for.

**What is in the 60,416 bytes:** the platform verifier (`rustls-platform-verifier` and its
`windows-sys` bindings), the WinINet proxy reader (`winreg`) and the gzip decoder (`flate2`
with `miniz_oxide`). The build was not profiled per crate, so the split is not measured —
the total is.

**What would pay it back, and did not happen here:** the binary still carries **Mozilla's
bundled roots** (`webpki-roots`) even though nothing uses them any more. They are only still
there because the **art fetches** (`art.rs:991`, `console_ui.rs:1320`) make their own
`ureq::get(...)` calls with the default configuration, which is `RootCerts::WebPki`. Move
those two to the shared client and the `rustls-webpki-roots` feature can go: the binary
loses the Mozilla roots — the crate is 447 KB of source, most of it certificate bytes — and
that is more than the 60 KB the platform verifier costs. That is a follow-up: it touches
files this phase was told not to touch.

**Rejected:**

- **Dropping the `gzip` feature** to claw the bytes back. The catalogue's payloads are
  1–2 KB, so gzip saves a kilobyte of traffic and costs tens of kilobytes of binary — a bad
  trade on its own — but the phase asks for compressed responses, and the self-test harness
  now proves that they arrive and are decoded.
- **Changing `strip` from `"debuginfo"` to `"symbols"`** to make the number fit. That is
  gaming the measurement: the binary would shrink for an unrelated reason and the symbol
  table the crash reports need would be gone.

---

## 13. Deliberately out of scope

- A public plugin SDK, a marketplace, a third-party publisher story.
- TUF, sigstore, TLS pinning, delta updates (`bsdiff` + `bzip2` — a 2.85 GB `target/`
  is a bigger problem than a 2 MB module payload).
- Crash reporting and telemetry. The product's promise is "everything is local, no
  telemetry" — keep it.
- **A Windows Service.** It would lose the GUI (session 0), gain a password-expiry
  failure mode, and need a `CreateProcessAsUser` bridge just to get the tray icon
  back. The per-user scheduled task is the right shape.
- Rewriting the pack format wholesale. `spec: 1` and `spec: 2` stay; `kind` and
  `triggers` are additive.
- Moving the engine's mode semantics anywhere new (see §2).
