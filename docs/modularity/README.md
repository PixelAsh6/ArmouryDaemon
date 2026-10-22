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

- **Phase A3 is merged into `main`** — the merge commit `21935ff`, and its CI run
  (`39803275300`) is green after 3m53s. The pack schema's `kind` and `triggers`, both
  **additive**, a malformed `kind` refused by name. The daemon does not act on the fields
  yet — the mode machinery is A4/A5 — so the phase changes no runtime path and the three
  shipped packs cannot notice it.
- **The review of 1.1.5 is closed on `fix/1.1.5-review`**, cut from that merge: the
  capability gate now covers the **declarative steps** (a pack that declared `["files"]`
  could run programs through a `cli` step), a cooldown change no longer resurrects a window
  that has ended, the overlay requests are coalesced, a denial inside a script-defined
  function is reported as a denial, and `audio_mute_all` leaves a crash-recovery record.
  **Not merged** — that merge is the user's call. CI: `39807213693` and `39809146891`,
  both green (378 unit tests, 133 harness checks). `DECISIONS.md` §20 (the step gate and the
  `registry` capability) and §21 (the game wins).
- Branch `main`, remote `https://github.com/PixelAsh6/ArmouryDaemon.git`. The phase is
  integrated on **`main`** — A2, with the lockout re-timing and the art fetch through the
  Windows certificate store merged into it — at the merge commit `4712b44`, and it is the
  **1.1.5** release. The branch it was built on, `feature/phase-a`, carries the **same
  tree** (`0791c0e`). An **A2-only** copy of the phase is still on
  `feature/a2-capabilities-clean`; the trees have **different numbers** — a number
  without its tree is not a number.
- The **1.1.5** release is out: the **capability-scoped host API**, the **Windows
  certificate store** for the catalogue **and the cover art**, and the **cooldown
  re-timing** — a change of the cooldown re-times the window that is running. It carries
  the executable (12,865,024 bytes) and its 85-byte sidecar, and nothing else. It is
  **published and installed** — the copy in `C:\Apps\ArmouryDaemon` hashes to
  `947ef3dc…24a4`, the published asset's hash.
- **The next action is Phase A4** — a separate worker queue for modes. The pack worker is
  a single serial thread (`packs.rs:168`), so a slow mode must never block a pack
  apply/revert: modes get their own thread and channel. `WORK-ORDER.md` A4.
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

**Phase A4 — a separate worker queue for modes.** The pack worker is a **single serial
thread** (`packs.rs:168`, `while let Ok(job) = jobs.recv()`), so a slow mode would block
every later apply and revert. Modes get their own thread and their own channel.
`WORK-ORDER.md` A4.

A3 is merged (`21935ff`), and the review of 1.1.5 is closed on `fix/1.1.5-review`; that
branch waits for the user's merge, and A4 starts from whichever tree the user wants. A3
put `kind` (`"pack"` or `"mode"`) and `triggers` in the schema; the daemon does not act on
them yet — that is A4 and A5. The full phase list is in `WORK-ORDER.md`. Phases run in
order: **A → B → C → D**, and **E (WASM) is deferred** until the rest has shipped and
settled.

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
