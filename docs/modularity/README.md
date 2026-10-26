# Modular modules — working folder

**Read this file first, every time you resume this work.** It is deliberately short:
after an auto-compaction it is the cheapest way to get your bearings back.

The task: make ArmouryDaemon modular. The user downloads **one** executable and, from
inside it, chooses which optional components to download:

```
console launcher · G-Helper · Rainmeter · Wallpaper Engine · idle mode · killswitch
```

Two rules that shape everything:

- The choice is made **at runtime, in the GUI** — not at install time, not at build time.
- **Nothing downloaded may run arbitrary code as Administrator.** The daemon runs
  elevated. This is the hard constraint; read `GOTCHAS.md` §7 before writing any of it.

---

## The files here

| File | What is in it | When to read it |
|---|---|---|
| **`README.md`** (this) | The index, the current state, the next action | **First, always** |
| `DECISIONS.md` | What is settled and what was rejected, with reasons | Before changing a design |
| `DESIGN.md` | The schema, the capability table, the manifest, the trust model | Before writing code |
| `WORK-ORDER.md` | The phases, each with exact file anchors and acceptance criteria | While working |
| `GOTCHAS.md` | Measured numbers, traps, and the house rules | Before you touch something unfamiliar |
| `PROGRESS.md` | The running log — **append to it after every phase** | Every session |

The deep research record is `docs/plans/2026-10-21-modular-modules.md` (on disk, not
versioned). Everything needed to do the work is in this folder; that file is the
background.

---

## Current state

- **Phase A4 is merged into `main`** (`cd4c3e3`, CI `40300606901` green): the pack
  worker is no longer a single serial thread. The plain packs keep their thread, the
  modes get a second one (`packs.rs:161`, `spawn_worker` :186), whole-state jobs
  split by the catalogue's `pack.kind` (`split_enabled`), tagged jobs follow the
  target pack's kind (`worker_for`), and the halves merge order-independently on the
  **existing** outcome channel through `PackOutcome::scoped`. The timing proof is
  harness scenario 19d (two 5 s modes, the plain pack unblocked both ways); the full
  story and the two red runs it took are the A4 entry of `PROGRESS.md`.
  `DECISIONS.md` §22.
- **Phase A5 is merged into `main`** (`e45b8be`, run `40307649812`, 4m40s; the
  branch's own runs - `40304545072` on `441c1b6`, `40306251783` on `806dd21` -
  are green too): the killswitch has a
  config key and an IPC request. `killswitch_enabled` (default `true`) is both the
  switch and the "is the killswitch module installed" flag: while it is off the End
  key and the tray toggle are logged no-ops and the tray item reads `Killswitch off
  in config`; `set_killswitch` over the pipe flips it; the state survives a restart;
  disabling releases an engaged killswitch first (windows back, audio back, tray
  icon back); and a status from an older daemon means on. The folded bug: `quit` now
  releases an engaged killswitch between the pack revert and the idle exit - the
  mutes live on the device, and before A5 the machine could stay hidden and silent
  after a quit. Harness scenario 19e pins it (8 checks). `DECISIONS.md` §23. CI
  green on the tip (`441c1b6`, run `40304545072`, 10m18s): **384 unit tests**
  (278 `armoury` + 106 `armoury-core`), **154 harness checks**, the exe
  **12,881,920 bytes** (sha256 `1005a3c5…091b`).
- **Phase A3 is merged into `main` and released in 1.1.6** — the merge commit `21935ff`,
  and its CI run (`39803275300`) is green after 3m53s. The pack schema's `kind` and
  `triggers`, both **additive**, a malformed `kind` refused by name. A3 changed no
  runtime path; `kind` is acted on from A4 on (it picks the worker) and `triggers` is
  still unread — no phase in the work order dispatches a mode yet, and A5 is the
  killswitch's config key and request.
- **The review of 1.1.5 is merged into `main`** (`95b0797`, CI `39816227849`, green after
  7m24s) and **shipped as 1.1.6**: the capability gate now covers the **declarative steps**
  (a pack that declared `["files"]` could run programs through a `cli` step), a cooldown
  change no longer resurrects a window that has ended, the overlay requests are coalesced,
  a denial inside a script-defined function is reported as a denial, and `audio_mute_all`
  leaves a crash-recovery record. `DECISIONS.md` §20 (the step gate and the `registry`
  capability) and §21 (the game wins).
- **1.1.6 is published and installed.** The tag `v1.1.6` is an annotated tag on `main` at
  `a61740f`; the release is
  `https://github.com/PixelAsh6/ArmouryDaemon/releases/tag/v1.1.6` (published
  2026-10-22 17:11:47Z). It carries **two assets and nothing else**: the executable
  (**12,879,360 bytes**, sha256 `0bc14524…56d4`) and its **85-byte** sidecar. Both were
  downloaded back and checked, and the published body is **2,987 bytes**, byte for byte
  the committed `release-notes/v1.1.6.md`. The copy in `C:\Apps\ArmouryDaemon` is that
  binary, the task is `Running` (last result 267009) with the new hash in its description,
  and the log reads `daemon starting (v1.1.6)` then `daemon ready`.
- Branch `main`, remote `https://github.com/PixelAsh6/ArmouryDaemon.git`. The phase is
  integrated on **`main`** — A2, with the lockout re-timing and the art fetch through the
  Windows certificate store merged into it — at the merge commit `4712b44` (1.1.5), and
  the review's fixes at `95b0797` (1.1.6). The branch it was built on,
  `feature/phase-a`, carries the **same tree** (`0791c0e`). An **A2-only** copy of the
  phase is still on `feature/a2-capabilities-clean`; the trees have **different
  numbers** — a number without its tree is not a number.
- The **1.1.5** release is out (the executable, 12,865,024 bytes, sha256
  `947ef3dc…24a4`, and its 85-byte sidecar) and **1.1.6 ships on top of it**: everything
  1.1.5 carried, plus Phase A3 and the review's fixes.
- **Phase A6 is merged into `main`** (`585f8ff`, run `40314663065`; the
  branch's runs - `40311776397` on `d172567`, `40313595169` on `7cdb0d2` -
  are green too): the exe carries its
  own C runtime. `.cargo/config.toml`'s MSVC rustflags gain
  `-C target-feature=+crt-static` beside the `target-cpu` pin — the work order's
  shorthand `-C target-crt-static` is not a real rustc flag — so the exe
  imports no `VCRUNTIME140.dll` and starts even on a stripped image; Cargo has
  no profile-scoped rustflags, so the debug builds go static with it.
  `DECISIONS.md` §24. CI green on the tip (`d172567`, run `40311776397`): the
  counts stand (384 unit tests, 278 `armoury` + 106 `armoury-core`, 154
  harness checks) and the artefact says **13,074,432 bytes** (+192,512, a much
  better price than the 1-2 MB estimate), `VCRUNTIME140.dll` absent,
  `kernel32.dll` still readable, sha256 `c1373e9e…22f8`.
- **Phase B1 is merged into `main`** (`e56a72a`, run `40320921365`; the
  branch's runs — `40318171903` on `5009452` green, `40319627691` on
  `0d0866d` green, one red on `7b80da3` — are recorded): the
  catalogue manifest reaches schema 2. The entries move under `modules` and
  each names its `kind` (`"pack"`/`"mode"`, validated by `PackKind::from_name`
  in `parse`; absent means `"pack"`), `Catalog::entries()` is the one way
  through, the old `packs` key keeps working for one release, and a manifest
  carrying both keys is refused. The release workflow's catalogue check learns
  `modules` with the same fallback; the packs repo's own `index.json` goes
  schema 2 in its repo's own commit, **pushed only at release time** — a live
  schema-2 manifest breaks the installed 1.1.6 daemons' fetches. `DECISIONS.md`
  §25. CI green on the tip (`5009452`, run `40318171903`, 10m42s; one red run
  before it — the probe's own server sent an RST, trap 53): **388 unit tests**
  (282 `armoury` + 106 `armoury-core`), **156 harness checks** (the schema-2
  probe lists through `modules`), the exe **13,080,064 bytes** (+5,632), sha256
  `97d436c4…5e66`.
- **Phase B2 is merged into `main`** (`e94110d`, run `40329394515`; the
  branch's green runs on `abf2754` and `f7b4ff1`, four red runs before the
  first one): the catalogue manifest arrives minisign-signed. `catalog::fetch` gets the
  manifest bytes, then `index.json.minisig`, verifies **before** parsing,
  and an unsigned or refused manifest is refused by name. `trust.rs` +
  `trust/keys.json` bake the keyring (`include_str!`, one trusted key today,
  two slots for the 90-day rotation); the keeper holds the matching private
  key and no repository ever does. The harness's local probes serve frozen
  byte-exact manifest+signature fixtures under `tools/fixtures/` (marked
  `-text`, signed once by a key whose private half died with its temp
  folder), the test ring trades in via `ARMOURY_TEST_TRUST`, and the real
  catalogue checks skip with a reason until the packs flip.
  `DECISIONS.md` §26. CI green on the tip (`abf2754`, run `40326319066`,
  11m38s; four red runs before it — the lockfile, the 0.3.0 API, a Debug
  impl, and the harness's second accept, traps 53-55): **394 unit tests**
  (288 `armoury` + 106 `armoury-core`), **156 harness checks** (the
  real-catalogue pair SKIP naming the unsigned live manifest until the
  flip), the exe **13,156,864 bytes** (+76,800), sha256 `0d5b48cf…787b`.
- **Phase B3 is merged into `main`** (`c8c0468`, run `40341551223`; the
  branch's green runs on `cc72986` and `74f855d`, two red runs before the
  first one): the catalogue's `helper` kind got its install
  path. `catalog::install` dispatches by entry - pack and mode keep the
  existing path, a helper extracts an archive into `modules/<id>/` behind
  its own folder lock (Administrators + SYSTEM full, the user read +
  execute, on `modules/` **and** the module folder - trap 56), records a
  `module.json`, and is rediscovered by a disk scan. New `helpers.rs`,
  `--module-list`, the `zip` 2.6 dependency, the strict manifest rules (a
  helper needs `entry`, a pack refuses it), and a 20d harness scenario
  serving manifest + signature + archive over one connection.
  `DECISIONS.md` §27. CI green on the tip (`cc72986`, run `40337739735`,
  13m28s; two red runs before it - the sibling module path and the UI
  test factory's initializer): **404 unit tests** (298 `armoury` + 106
  `armoury-core`), **160 harness checks**, the exe **13,275,136 bytes**
  (+118,272), sha256 `4e591518…64a3`. The branch is in `main` at
  `c8c0468`, and the next work is B4.
- **The next action is Phase B4** - the "Modules" window
  (`WORK-ORDER.md` B4): the rows, the shapes and the
  download/verify/install flow behind the daemon. B3 is in `main` at
  `c8c0468`; the packs flip, the signing key and the next release are the
  user's calls.
- The **1.1.4** release is out: the lockout rewritten to hold a change in both directions,
  the release carrying only the executable and its sidecar, the toolchain pinned. It is
  committed on top of `0350aac` (version **1.1.3**, released).
- **Phase A1 is done** — the catalogue is validated against the **Windows certificate
  store**, not Mozilla's bundled roots, so it works behind a corporate CA. The client is
  one `ureq::Agent`, the user's WinINet proxy settings are read, and the responses are
  compressed. See `PROGRESS.md`.
- **Phase A2 is done** — the host API is **scoped by capability**. A pack declares the
  capabilities it wants; the daemon keeps **one engine per capability set** and
  registers only the granted functions on it, so a call to a function the pack was not
  granted fails as an *unknown function*. The denial names the missing capability and
  what the pack asked for, and it lands in the pack's log. The host functions report
  what they changed: `audio_mute` returns the **previous mute state** of every session
  it touched and never unmutes a session the user had muted. See `PROGRESS.md`.
- ~60 % of the request is **already shipped**: G-Helper, Rainmeter and Wallpaper
  Engine are data packs that the catalog already downloads, verifies and installs.
- The gap is the **killswitch**, **idle mode** and the **console launcher**.

## What 1.1.3 and 1.1.4 already did (do not redo it)

| Done | Effect | Where |
|---|---|---|
| The script sandbox keeps **one engine per capability set** (since A2); budgets re-armed per run | 432.7 µs build cost paid once; a run spends 4.7 µs | `crates/armoury/src/script.rs:90` |
| The killswitch enumerates audio sessions **once per press**; release restores from the ledger without enumerating | ~45 ms off the event loop per press | `crates/armoury/src/killswitch.rs:121`, `:415` |
| The watcher **skips the process-table walk when nothing is tracked** | ~19.5 ms/s of idle cost gone | `crates/armoury/src/watch.rs:174` |
| The **lockout model was rewritten** — again on 2026-10-22: it holds a change in **both** directions | the packs no longer flip under a running game, and a start inside a window no longer changes the machine in the middle of a cooldown | `crates/armoury-core/src/engine.rs` (`try_change` `:314`, `tick` `:291`) |
| A change of the cooldown **re-times the window that is running** (1.1.5) | the window keeps its start and takes the new length, so it ends at `start + cooldown` and the change it holds lands at the new expiry; a window that has **already ended** is dropped instead (2026-10-23) | `crates/armoury-core/src/engine.rs` (`Window`, `set_cooldown`) |
| The **release carries only the executable and its 85-byte sidecar** — no zip, no scripts | the executable installs itself; the scripted install lives in the repository's `deploy\` folder | `.github/workflows/release.yml` |
| The **build targets x86-64 v3**, not `native` | the same binary on every machine | `.cargo/config.toml` |

---

## The next action

**Phase B4 - the "Modules" window** (`WORK-ORDER.md` B4): the rows and the
shapes (pack/mode/helper), the per-module capability display, and the
download/verify/install flow behind the daemon. B3 is on its branch
`feature/b3-helpers` until its runs are green - then the merge; the packs
flip, the signing key and the next release are the user's calls.

Phases run in order: **A → B → C → D**, and **E (WASM) is deferred**
until the rest has shipped and settled.

---

## How to keep this folder alive

The point of the folder is to survive an auto-compaction, so:

1. **Append to `PROGRESS.md` at the end of every phase** — what you did, what you
   measured, what is left, what you would do differently. Write it for someone who
   has no memory of the session: state, next step, and anything surprising you found.
2. **Update `PROGRESS.md`'s "current state" block at the top when you finish a phase**,
   and mirror the change into this `README.md`'s "Current state" and "Next action".
3. **If you change a design, change `DESIGN.md` in the same commit** — not later.
4. **If you find a trap, write it in `GOTCHAS.md` immediately**, while you still have
   the context. That is the whole reason the file exists.
5. **Do not let this folder and `docs/plans/2026-10-21-modular-modules.md` drift
   apart.** The folder is the versioned, operational truth; the plan file is the
   background. If they disagree, the folder wins — and fix the plan file.

## House rules (the short version — the full list is in `GOTCHAS.md`)

- **All building happens in GitHub Actions.** Do not run `cargo build`, `cargo test`
  or `rustc` on this machine — not even a parse-only invocation. Push the branch and
  watch it: `gh run watch` (the `gh` CLI is installed and authenticated as PixelAsh6).
- Commit as the user: `PixelAsh6 <PixelAsh6@users.noreply.github.com>`, author **and**
  committer. No AI identity anywhere — not in commits, code, comments or docs.
- **Never run `cargo fmt`.** The repo has no rustfmt pins and it reflows everything.
  Format by hand.
- Keep the promise: everything is local, no telemetry.
