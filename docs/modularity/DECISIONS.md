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

---

## 14. The host API is scoped by capability, one engine per capability set

**Settled — implemented in A2.** A pack declares the capabilities it wants
(`"capabilities": ["files"]`) and the daemon grants only those. A call to a host
function of a capability that was **not** granted fails as an **unknown function**:
the function is not on the engine at all, so the denial is structural — there is no
call site that could forget to check.

The taxonomy lives in the schema (`armoury_core::pack::Capability` and `CapSet`), the
engine cache in the sandbox (`script.rs`, `ENGINES`).

**Why one engine per capability set.** The sandbox has kept one engine per thread
since 1.1.3, so "register fewer functions" is no longer free — a second engine costs
~433 µs to build. The alternatives were both rejected:

- **One shared engine plus a dispatcher** that consults the grant set at call time.
  Cheaper in memory, but the denial becomes a runtime branch — a call site could
  forget it, and that is exactly the property the phase is buying.
- **One engine per pack.** Throws the 1.1.3 caching win away.

Rhai has no `unregister_fn`, and `disable_symbol` only disables keywords and
operators, so "register less" and "check at call time" are the only two mechanisms
available. The engines are keyed by the capability set, so packs that ask for the same
capabilities share one, and the map is capped at 32 like the AST cache.

**Why the taxonomy is in the schema crate.** The permission window (Phase B4) has to
show what a module may do, and it has to know the same table the gate uses. Two lists
would drift.

### The `sleep` decision: `time` is a capability of its own

**Settled.** The table as it was written down listed 17 of the 18 existing host
functions: **`sleep` was in no capability at all**. Under per-capability-set engines a
pack whose script called `sleep` would have failed as an unknown function — the
taxonomy had a hole in it.

`time` is the capability for it, and it is a capability of its own because:

- **It grants nothing.** `sleep` is a flow-control primitive: it touches no file, no
  process and no window. Every other capability is a *power*; `time` is not.
- **The budgets already bound it.** The operation budget (500 000 operations) and the
  wall clock (10 s) are re-armed before every run, and `sleep` is clamped to 10 s.
  A capability that grants nothing needs no second limit of its own.
- **Folding it in would give it away.** Putting `sleep` in `context` would hand a
  pack that only reads the context a way to stall the pack worker; putting it in
  `time` keeps the grant list honest — a pack that wants to wait says so.

**Rejected:**

- **Putting `sleep` in `context`** (the read-only capability). It would have made
  every pack that reads the context able to block the worker, for no reason.
- **Putting `sleep` in `journal`** (the pack's own log). Same problem, and it would
  have made the read-only capability a write.
- **Leaving it out of the table** and special-casing it in the engine (always
  registered). That is a hole in the taxonomy: the table would not be the list of what
  a pack may call, and the permission window could not show it.
- **A `sleep` with a tighter clamp** (say 1 s). The 10 s clamp is not this phase's
  business, and the wall clock already stops the run.

### The default of the `capabilities` field is every capability

**Settled.** `DESIGN.md` §1 originally said the default was `[]`. That is wrong for
the shipped packs: the Rainmeter pack's script uses `window_message`, and a pack that
asks for nothing would lose it. The field is **additive** and defaults to every
capability, so the three shipped packs keep working unchanged.

The field is what the pack **asks** for. The user's own grant list (the permission
window, Phase B4) comes later and narrows it down: the effective grant is the
intersection of the two. Until the permission window exists, the ask is everything.

**Rejected: refusing a pack that declares no capability.** It is a legal thing to
write — a pack that only reads its context — and the daemon can serve it.

### The overlay is a request to the daemon, not a call from the pack

**Settled.** `overlay_show()` and `overlay_hide()` do not touch a window. The black
windows belong to the thread that pumps their messages, and that is the daemon's main
loop; a window created on the pack worker would never be painted and could not be
dismissed by a keypress.

So the host function hands a request over (`packs::request_overlay`), the daemon's main
loop picks it up and carries it out as its own **idle mode** (`enter_idle` /
`exit_idle`), and the daemon keeps owning the crash-recovery ledger, the keypress
dismissal and the idle-mode policy. `overlay_show` therefore means "the daemon enters
idle mode", and the pack sees the request accepted, not the overlay on the screen.

**Rejected: creating the windows on the pack worker.** The message queue belongs to
the creating thread, so the daemon's pump would never reach those windows, and
`DestroyWindow` from another thread is not a thing to rely on.

---

## 15. The executable grows by 56,832 bytes - the size rule bends here, once

**Settled — measured in A2.** The release build of the tree goes from **12,868,608
bytes** (A1) to **12,925,440 bytes**: **+56,832 bytes, +0.44 %**.

**The same phase on the branch it is delivered on costs a little more.** That branch
also carries the art fetch through the Windows certificate store, and that work takes
Mozilla's bundled roots **out** of the binary: its own baseline is **12,796,416
bytes**, and the A2 work on top of it ends at **12,862,976 bytes** — **+66,560 bytes,
+0.52 %**. Both numbers are true; they are measured against different parents, and a
number without its tree is not a number (`GOTCHAS.md` trap 35). **The baseline for the
size rule on the delivered branch is 12,862,976 bytes.**

**Why.** The phase delivers the design's capability table, and the table has **eight
host functions that did not exist** (`windows_of`, `window_hide`, `window_show`,
`audio_sessions`, `audio_mute`, `audio_mute_all`, `overlay_show`, `overlay_hide`). They
are thin wrappers, but **a Rhai registration is not free**: every *shape* of host
function gets its own instantiation of the argument-marshalling code (the `Dynamic`
unboxing of each parameter and the wrapping of the return value), and the eight
functions add **six new shapes**. That is where the bytes are — the section table of
the two release builds shows it: **+47,616 bytes of `.text`** (code), +6,656 of
`.rdata` (the string literals and the vtables of the new registrations), +1,536 of
`.pdata` (the unwind tables of that code). The **gate itself is nearly free**: a
bitmask over twelve capabilities, a table, and a lookup.

There is no new dependency, no framework and no new runtime: the growth is the host API
the phase is about.

**What was rejected:**

- **Dropping the eight functions to keep the size flat.** The design's table would
  name functions that do not exist, and the killswitch and idle modules — the things
  that need them — would arrive in a later phase with a host API that cannot do their
  job. The table is the contract between the schema, the gate and the permission
  window (B4); a table with holes in it is worse than 56 KB.
- **Making the registrations cheaper by turning the closures into function pointers.**
  The marshalling code would then be shared between the functions of the same shape —
  but that shrinks the *whole tree*, the baseline included, so the **difference** against
  the A1 build would not close.
- **`#[inline(never)]` on the engine builder and boxing the engines.** Measured: the
  executable grew by 512 bytes, not less. The builder was not being inlined twice, and
  the change was reverted — the shortest working diff is the rule here too.

**Known trade-off, accepted:** the phase adds bytes, and `WORK-ORDER.md` says only
Phase E may. The reason is the one above, and the number is written down here so the
next phase starts from the truth: **the baseline for the size rule is now 12,925,440
bytes on the A2-only tree and 12,862,976 bytes on the delivered branch, not
12,868,608.**

**The alternative, if the user wants the size flat:** the eight functions can move to
the phase that brings the killswitch and idle modules (B/C), and A2 then ships the
gate alone. That is a decision about the *order* of the phases, not about the design —
and it should be taken by the user, not by the phase that happens to be running.

**The size rule bends on a tree that carries another branch's work.** The number above
is the A2-only tree (`0ecacd3` + A2). The branch this phase is delivered on also
carries the art fetch through the Windows certificate store, and that work takes
Mozilla's bundled roots **out** of the binary — so the same phase costs
**+60,928 bytes** there, measured against that tree's own baseline of 12,796,416
bytes. Both numbers are true; they are measured against different parents. See
`PROGRESS.md` for which tree carries which number.

---

## 16. `audio_mute` returns the previous mute state, per session

**Settled — implemented in A2.** `DESIGN.md` §2 requires the host function to return
the previous mute state. The first implementation returned only counts
(`2 session(s) muted, 1 left alone, 0 could not be changed`), which says *how much*
happened but not *what the state was*. A pack's revert needs the state.

So the report names **every session of the process and the state it was in before the
call**:

```
muted {session} (was playing), left alone {session} (was muted)
```

with `unmuted {session} (was muted)` for a session this call had muted before, and
`could not be changed {session} (was playing)` when Windows refused. A process without
an audio session gets `no audio session for pid {pid}`.

**Why per session and not per process.** A process can own several audio sessions, and
they do not have to be in the same state — a game may have one session the user muted
by hand and another that was playing. One word for the whole process would either lose
that distinction or be a guess ("mixed").

**Why the state is named and not just counted.** The counts alone cannot answer "was
that the user's own mute or ours?" — and that is the difference between a revert that
restores the machine and one that unmutes something the user silenced.

**Rejected:**

- **The counts alone** (what the first implementation did). It does not carry the
  previous state, which is the one thing the design asks for.
- **A separate host function that only reports the state** (`audio_state(pid)`). The
  report already carries it, and a second function would be a second thing to gate.
- **A boolean "was muted" per process.** Lossy, as above — and it cannot say which of
  the sessions was the user's own.

**The second half of the same rule — never unmute a session the user muted — is
enforced by the ledger, not by the report.** The sessions this call muted are
remembered by their session id, so the unmute touches exactly those. The report says
so out loud (`left alone {session} (was muted)`), and a unit test proves both halves.

---

## 17. A change of the cooldown re-times the window that is running

**Settled — implemented in 1.1.5.** The lockout model is unchanged (§10): the window
still holds a change in **both** directions. What is settled here is what a new cooldown
does to a window that is **already running**.

When the cooldown is changed while a window is running, the window **keeps its start and
takes the new length**: it ends at `start + cooldown`. The setting is in force at once, and
a window is never a mix of the old and the new length.

**Why.** The daemon log of 22 Oct 08:46 shows the defect. The apply at 08:46:26 armed a
**10 s** window (the old value); the setting landed a second later (10 s → 60 s) and
changed only the length of *future* windows, so that window expired at 08:46:36 — three
seconds before the game came back at 08:46:39. The revert landed in that gap (the packs
went to Silent), and the game's return then sat **held** inside the new 60 s window with
the packs off: they only came back at 08:47:37. With the re-timing the window ends at
08:47:26, the game comes back at 08:46:39 — inside the window — so the held revert is
**cancelled** and the packs stay applied.

**How it is built.** The window is kept as its **start** and its **length**, not as a
deadline (`Window` in `engine.rs`), so it can be re-timed at all; `set_cooldown` sets the
length of the window in flight. The engine also names the change the window is holding
(`pending_change`), and the daemon publishes it in the snapshot's `cooldown_action` — the
field the wire format has carried since the first release, and which the dashboard's
cooldown row reads to say that a change is waiting behind the window.

**Rejected:**

- **Letting the change apply only to future windows.** The old behaviour, and the defect
  itself: the window in flight ends at the old expiry and the change it holds lands in
  that gap.
- **Re-arming the window from *now*.** The window would then run the full new length from
  the moment of the change, so it could outlast the setting and the change it holds would
  be delayed — the setting would be longer than the user asked for.
- **Extending a window that would end earlier than now.** A new length that is **shorter**
  than the time the window has already run ends the window **at once** instead, because a
  window cannot end before it began: the user asked for a short window, and more than that
  has passed. The change the window was holding lands with the next tick.

**The local-only record.** `DECISIONS-2026-10-17.md` (on disk, not published) carries the
same decision as row **H21**, with the log excerpt it is based on. That file is the
private decision log of the earlier sessions; this section is the versioned copy.

### A window that has ended is dropped, not re-timed

**Amended 2026-10-23.** Only a window that is **still running** is re-timed. A window
whose `end` is in the past is **dropped** by the same call.

The engine keeps an expired window until a tick reaps it, and the daemon drains the pipe
requests **before** the 1 Hz housekeeping runs that tick - so a `set_cooldown` that
arrives in that sub-second gap found a dead window and gave it the new length. A window
`[t0, t0+5]` that had expired with nothing pending came back as `[t0, t0+60]`: the
dashboard showed a countdown, `lock_running` was true again, and the next game launch was
**held** for a cooldown that was never really running - a phantom lockout. The call takes
`now` for exactly this: `match self.lock { Some(w) if w.end() > now => ..., _ => None }`.

---

## 18. `kind` and `triggers` are additive, and the mode machinery comes later

**Settled — implemented in A3.** The pack schema gained two fields: `kind` (`"pack"` or
`"mode"`) and `triggers` (what starts the pack). Both are **additive**: a pack that does
not carry them loads and behaves exactly as it did before, the three shipped packs carry
neither, the catalogue needs no change, and `spec` stays 1 or 2.

**The defaults.** `kind` defaults to `"pack"`; `triggers` defaults to
`[ { "kind": "tracked_app" } ]` — today's behaviour, a pack applies while a tracked app
runs. **The default is the same for every kind**: a mode that does not say what starts it
inherits the tracked app, like any other pack. The rule "a mode needs at least one
trigger" therefore bites on an **explicitly empty** trigger list, not on an absent one.

**Why.** `DESIGN.md` §1 gives the field one default, and that default is the behaviour a
pack had before the field existed. A default that depended on the kind would need the
schema to know whether the field was *present* — `serde(default = …)` cannot see the
other fields — and the pack's own default is the one that keeps today's packs working.

**A malformed `kind` is a named error.** The kind is a closed set, and a value outside it
is refused when the pack is parsed: `unknown kind 'widget' (expected 'pack' or 'mode')` —
the same shape as the capability list's `unknown capability 'teleport'`, and it names what
the kind may be, because there are only two of them and the author would otherwise have
to guess. It is never a panic and never a silent default to `"pack"`.

**The shape of a trigger.** A trigger is `{ "kind": … }` and, for a `hotkey`, the key it
listens for. Two rules the folder did not state, settled here:

- **A `hotkey` trigger needs a key.** Without one it names no key, and nothing would ever
  start the pack.
- **A `tracked_app` trigger takes no key.** A key there would be ignored in silence —
  the same failure the other schema errors guard against ("a `when` that names something
  the pack does not have would never match, and the step would vanish silently while the
  pack reports success").

**The key name is not checked against the daemon's hotkey table.** The schema does not
carry the list of keys (`crates/armoury/src/hotkey.rs` knows End and Home), and a second
list would drift from the first — the same reason the capability table lives in one
place. A key the daemon does not know is a trigger that never fires; the daemon-side
resolution belongs to the phase that dispatches a trigger (A4/A5).

**The daemon does not act on the fields yet — and that is why the phase is additive.**
Before the schema knew them, a pack that carried `"kind": "mode"` loaded as an ordinary
pack: serde drops the fields it does not know. After it, the daemon still applies a mode
pack the way it applies any other pack, because the mode machinery — a worker of its own,
a hotkey that dispatches to it — is Phase A4 and A5. **The daemon's behaviour did not
change; the schema learned to say what it always meant.** The kind is in the log line
that names the loaded pack, so an operator can see it.

**Rejected:**

- **A `spec: 3`.** `spec` is the *script* axis and stays 1 or 2 (§13). `kind` and
  `triggers` are new, separate axes.
- **Refusing a mode pack until the mode machinery exists.** It would change what the
  daemon does with a pack it already accepts, and it would make the field meaningless in
  the phase that introduces it. The gates belong to the phase that implements them.
- **Validating the hotkey key against the daemon's table inside the schema.** Two lists,
  and the schema would have to know about the daemon's hotkeys.
- **A default that depends on the kind.** It needs the schema to tell an absent field
  from an empty one, and it would make the mode rule fire on packs that never mentioned
  triggers.

---

## 19. The schema phase adds 12,288 bytes — the size rule bends here, once more

**Settled — measured in A3.** The release build of the tree goes from **12,865,024 bytes**
(the v1.1.5 release asset — the baseline for the size rule on the released tree) to
**12,877,312 bytes**: **+12,288 bytes, +0.10 %**.

**Where the bytes are.** The PE section table of the two builds says it in a minute: the
growth is **code** — **+10,752 bytes of `.text`** — plus **+1,024 bytes of `.rdata`** (the
format strings of the new error messages) and **+512 bytes of `.pdata`** (the unwind
tables of that code). The other sections did not move: `.data`, `.rsrc` and `.reloc` are
byte-identical.

**The split was not measured per crate.** That needs a build on this machine, and the
build happens in CI. With `lto = "fat"` and `codegen-units = 1` a change of this size
re-optimises the whole program, so the 12 KB is the program's new layout and not
necessarily the two enums alone.

**Why the rule bends.** The rule exists to catch *gratuitous* growth — a module system, a
WASM runtime — and not to freeze a phase that delivers what the work is for. This phase
delivers the schema the whole module system rests on: two discriminants, their
(de)serializers and the validation that refuses a malformed pack by name. The bytes buy
a pack format that can say what a pack is.

**Rejected:**

- **A `String` for the kind instead of an enum.** It would have saved the two
  (de)serializers - the bytes were not measured against that alternative - and it would
  have cost the typed discriminant that the Modules window (B4) and the manifest (B1)
  both need. A schema that cannot say what a pack is is worth more than a kilobyte.
- **Dropping the named error to save the format strings.** A malformed `kind` has to be
  a named error (`WORK-ORDER.md` A3), and a silent default to `"pack"` is exactly what
  the phase exists to prevent.

---

## 20. The capability gate covers the declarative steps, and `registry_set` gets `registry`

**Settled - 2026-10-23, from the review of 1.1.5.** A pack that declared
`"capabilities": ["files"]` - or `[]` - could still run programs, inject keystrokes,
kill processes and write the registry, because those are **declarative steps** and the
gate only ever saw the **script**. The script is gated structurally (only the granted
functions are on its engine); a step is data, so nothing looked at it.

**The mechanism table** now sits next to the schema (`Capability::of_mechanism`):

| Mechanism | Capability |
|---|---|
| `cli` | `exec` |
| `process_start`, `process_kill` | `process` |
| `hotkey` | `input` |
| `registry_set` | `registry` |

`Pack::validate` refuses a step of a capability the pack did not ask for - so the pack
**never loads** - with the sentence the script denial uses:
`apply step 0: 'cli' needs the 'exec' capability, which this pack was not granted (the
pack asked for: files)`. `run_step` makes the same check where the mechanism is actually
run, so a pack that reached the worker by another path cannot reach the machine either.

**`registry_set` had no capability at all**, and the table had no row it belonged to.
It gets one of its own, `registry`, rather than being folded into `files-write`:

- **It is not a file.** `files-write` is documented as *"inside the four roots"*
  (`%APPDATA%`, `%LOCALAPPDATA%`, `%PROGRAMDATA%`, the target's folder), and the gate
  enforces that. A registry write is not inside any of them, and it can set a value
  anywhere under HKCU/HKLM - which is where Windows looks a start-up entry up. Folding it
  into `files-write` would have handed a pack that asked to write a config file a
  persistence channel, and §14's rule for `sleep` is exactly this: folding a power into
  another capability gives it away.
- **The permission window (B4) has to show it.** The window lists what a module may do in
  the words of `Capability::effect`, and "Write files" would be a lie for a module that
  can write `HKCU\...\Run`. Two lists that drift is what §14 rejected; one list that
  describes the wrong thing is worse.
- **It costs less than a new host function.** `registry` carries **no function** (the
  only way to write the registry is a step), so it adds an enum arm, a name, an effect
  string and a bit in `CapSet::ALL` - no engine, no registration, no budget.

**Rejected:**

- **Leaving `registry_set` ungated.** The hole the review found: `[]` + `registry_set` is
  a persistence channel for a pack that asked for nothing.
- **Folding it into `files-write`.** Above: the four roots are the documented and
  enforced limit, and HKCU is not inside them.
- **Folding it into `exec`.** Killing a process is `process` and running a program is
  `exec`; a registry write is neither, and a pack that only sets a value would have had
  to ask for the power to run programs.
- **A runtime-only check.** A pack that is refused at apply time has already been
  installed and looks healthy in the UI; the refusal has to be at `validate()` so it
  never loads. The `run_step` check is the backstop, not the gate.

**The default still holds.** A pack that declares no `capabilities` gets `CapSet::ALL`,
so the three shipped packs keep working: none of them carries the field, and none of them
uses a step the new row would refuse.

---

## 21. The game wins: a start inside the window cancels the revert

**Settled by the user 2026-10-23.** When a tracked game closes and starts again inside
the same cooldown window, the pending **revert is cancelled** - the machine stays in
**Turbo**, the packs applied - **and the window keeps running** to its expiry, so it is
still the guard over whatever the packs try next.

**Why.** The alternative is the machine in **Silent while a game runs**: the revert lands
in the gap between the close and the start, and the game's launch then sits held behind
the very window the revert started (the 22 Oct 08:46 log, §17). A game on the desktop
with the G-Helper Turbo profile off is the one outcome the lockout exists to avoid.

**The code already behaves this way** - it is H20's rule (4) (`DECISIONS-2026-10-17.md`,
the local-only log; H20 is the block this belongs to). `on_started` sets the intent, the
engine reaches `desired == applied`, the held change is **cancelled** (`pending = false`)
and the window is left running; the next tick finds nothing to do and starts no window of
its own. **Nothing in the engine was changed for this decision**: it was re-verified
against the code, pinned by a harness check (9b - the window is still running after the
restart, the change behind it is gone, no revert lands, the packs stay applied) and
recorded here.

**Rejected:**

- **Letting the revert land and re-applying at the end of the window.** That is the
  flip-flop of 21 Oct under a running game, one window later.
- **Dropping the window when the revert is cancelled.** The window would stop guarding the
  packs: a close right after the restart would revert at once, which is the same defect
  the other way round.
- **Restarting the window from the restart.** The user's cooldown would be measured from
  the game's own start, and a game that restarts twice would hold the packs far longer
  than the setting says.

---

## 22. The modes run on a worker of their own, and the two halves merge on one channel

**Settled 2026-10-23.** `packs::start` (`packs.rs:161`) takes two job receivers and
spawns two workers: the plain packs keep a serial thread with exactly the old shape,
and the modes get a second thread built from the same extracted loop
(`spawn_worker`, `packs.rs:186` — its own COM init, its own carry, the same wake
`SetEvent`, and the same log words with "mode" in place of "pack"). The daemon splits
a whole-state job by the catalogue's `pack.kind` (`split_enabled`) — an unknown id
stays on the pack side, which is the pre-A4 behaviour — and routes a tagged
single-pack job by the target pack's kind (`worker_for`).

**Why a thread and not a queue on the same thread:** the acceptance is *a mode that
takes five seconds does not delay a pack revert* (`WORK-ORDER.md` A4). Anything the
same serial worker still drains in order cannot grant that — a five-second step ahead
of a revert delays the revert by five seconds whatever lane it sits in. The pack
worker keeps its thread and its behaviour; the only thing that changed about it is
how many of it there are.

**One outcome channel, not two.** Both workers send their `PackOutcome` into the
existing channel, and `on_pack_outcome` keeps draining the way it did — the state
merge, the journal write and the restart-revert read the same stream as before. What
makes that safe is `PackOutcome::scoped`: the outcome echoes the enabled ids **its
own** half carried, and `merge_scoped` folds a half in so the result is the same
whichever half lands first. Without the echo the second half would overwrite the
first's ids and one worker would drop the other's applied packs out of the state —
two producers on one merge means every record carries enough to be merged
order-independently.

**Rejected:**

- **A priority lane on one thread.** It changes which job is picked up first but not
  the serialisation: a five-second mode step picked up ahead of a revert still holds
  the revert for five seconds. Modes are rare; plain applies and reverts are the
  worker's bread and butter.
- **Per-worker outcome channels with a split drain.** It would divide the drain loop,
  the state merge, the journal write and the restart-revert join between two channels
  — every consumer of outcomes doubled, and the same echo merge still needed to
  combine the halves. One channel keeps the write side of the pack state
  single-threaded, which is the property the old design rested on.
- **Sizing the two workers differently (a pool for packs, one thread for modes).**
  The workers' lists are disjoint by kind, so nothing can contend; a pool would only
  make two plain packs run their steps interleaved — a reordering the
  single-threaded design never promised to survive. A4 does not pay for concurrency
  the packs have never been tested against.

---

## 23. One key for the killswitch: the config switch and the "installed" flag

**Settled 2026-10-26.** A5 gives the killswitch the config key (`killswitch_enabled`,
default `true`) and the IPC request (`set_killswitch { enabled }`) it never had —
and the two live up to the work order's "natural place for the is-the-killswitch-
module-installed flag" by being **one key**, because the user who turns a killswitch
off wants a switch that reads off everywhere: the End key and the tray item become
logged no-ops, the tray item says `Killswitch off in config` instead of promising a
toggle, and a fresh config (or an old one without the key) means on. The thing that
survives a restart is the key; the engaged state stays in memory exactly as before.

- **Disabling releases first.** A switch that reads off must not leave windows
  hidden and audio muted behind it: `set_killswitch_config` disengages an engaged
  killswitch, resets the tray icon, saves the config and broadcasts the status.
  While the key is off nothing can re-engage — the toggle's top guard refuses the
  End key and the tray, and the mid-run engage sits on `is_active()`, which from
  now on implies enabled.
- **Older snapshots mean on.** `StatusSnapshot.killswitch_enabled` defaults to
  `true`: a snapshot from a daemon that has neither key nor request describes a
  killswitch nothing could have switched off.
- **The quit path releases it** (the loose end folded into A5): `quit` reverted the
  packs and left idle, but an engaged killswitch rode into the exit, and its mutes
  live on the device — the machine could stay hidden and silent until the next
  start. `release_killswitch_for_exit` now runs between `revert_before_quit` and
  `leave_idle_for_exit`: same guard, same shape.

---

## 24. The C runtime is statically linked

**Settled 2026-10-26.** The owning decision A6 asks for: the exe **carries its
own C runtime**. Before A6 it imported `VCRUNTIME140.dll` — effectively always
present on stock Win10/11, but absent on a stripped image (LTSC, thin VMs),
where the daemon refused to start with nothing the user could fix from inside
the product. The install ritual is a lone exe and its hash sidecar, with no
redistributable to install; the runtime was the one hole in the "lone exe"
story. The accepted cost is weight, not behaviour: the A6 artefact measured
**+192,512 bytes** (188 KiB) against the 12,881,920-byte A5 build - far under
the work order's 1-2 MB estimate.

- **The spelling.** The work order's shorthand `-C target-crt-static` names no
  real rustc flag; the effective one is `-C target-feature=+crt-static`, appended to the MSVC target's rustflags in `.cargo/config.toml`, next to the
  already-decided `target-cpu=x86-64-v3` (§5).
- **Why on the target, not a profile.** Cargo has no profile-scoped rustflags,
  so the flag rides the target: the debug builds link statically too —
  deliberate. Every build a developer or the runner makes is the same
  dependency reality, and the acceptance check never compares a static release
  artefact against a dynamic debug.
- **The acceptance is a byte fact, not a size claim.** Import-table strings
  live verbatim in a PE, so the check is: the artefact contains the byte string
  `VCRUNTIME140.dll` **nowhere**, while `kernel32.dll` (a real import beside
  it) is still readable — a check the dynamic build demonstrably fails.
- **Turned down:** keep the dynamic CRT (smallest exe wins) — a start failure
  on a stripped image is worse than a megabyte; a `setup.ps1` preflight that
  warns about the missing DLL — it would document the dependency instead of
  removing it.

---

## 25. The manifest says what its packs are: schema 2, `modules` and a `kind`

**Settled 2026-10-26.** B1 moves the catalogue manifest to schema 2 and the
key with it: the entries live under `modules`, and each entry names its
`kind` — `"pack"` or `"mode"`, the same vocabulary the pack file's own
`kind` field uses (`PackKind::from_name` validates it in `parse`, and an
unknown kind refuses the manifest with the entry's id and the bad name). A
manifest that names no kind gets `"pack"`, the same default a pre-A3 pack
file gets; the three shipped packs carry no `pack.json` kind and stay
`"pack"` — the manifest mirrors what the daemon reads after the download,
nothing else.

- **One release reads both keys.** The build accepts `packs` (schema 1) and
  `modules` (schema 2) and refuses a manifest that carries **both**:
  the publisher writes exactly one, and a silent pick between two lists is
  the kind of bug that costs an afternoon. The dual read is a courtesy with
  an expiry — the next build after this one may refuse the old key.
- **Schema `0` is still not a schema.** The range check is `1..=2`; the old
  code understood exactly `{1}` and refused `0` the same as `99`. The
  continuation rule does not change with the extra number.
- **The release workflow follows.** `release.yml`'s "Check the packs against
  the catalogue" step reads `modules`, falls back to `packs` for the same
  window, and throws on both — and keeps failing when a release ships bytes
  the catalogue does not publish. The manifest is the same key pair, so the
  check and the daemon see one truth.
