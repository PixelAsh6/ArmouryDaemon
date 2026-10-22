# Work order

Phases run **in order**: A → B → C → D. **E (WASM) is deferred** until the rest has
shipped and settled. Finish and verify each phase before starting the next.

> **All building happens in GitHub Actions.** Do not run `cargo build`, `cargo test`
> or `rustc` on this machine — not even a parse-only invocation. The release profile
> takes 13–21 minutes of the user's CPU; a runner does it for free and identically.
> Push the branch and watch the run.

## How to verify a phase

```powershell
git add -A; git commit -m "..."
git push origin <branch>
gh run watch          # or: gh run list --limit 5
gh pr create --fill   # if you want the run on a PR
```

The CI workflow (`.github/workflows/ci.yml`, Windows runner, 90-minute timeout) does:
build the workspace → check the executable carries its manifest → run the unit tests →
check the shell is elevated → run the self-test harness. A release build happens on a
tag push.

**Extend `tools/daemon-selftest.ps1` for every new behaviour.** It runs in CI, so it is
the real acceptance test for the daemon's behaviour. It has already caught two real
bugs. It launches a throw-away daemon with an env-namespaced pipe/config/mutex.

**After every phase: append to `PROGRESS.md` and update `README.md`'s "Current state"
and "Next action".**

---

## Phase A — foundation (no user-visible change)

### A1. Fix the TLS root store — **done (2026-10-22)**

A real bug, and it blocked the whole module channel. It is fixed: the catalogue is validated
against the Windows certificate store now.

`Cargo.toml` had `ureq = { version = "3", default-features = false, features = ["rustls"] }`.
In ureq 3.4.2 the `rustls` feature is
`["rustls-no-provider","_ring","rustls-webpki-roots"]` and `RootCerts` defaults to
`WebPki`. `Cargo.lock` contained `webpki-roots` and **no** `rustls-native-certs`. So the
catalog fetch validated TLS against Mozilla's bundled roots, **not the Windows cert
store** — and it failed on any machine with a corporate CA or TLS interception.

```toml
ureq = { version = "3", default-features = false,
         features = ["rustls", "platform-verifier", "win-system-proxy", "gzip"] }
```
```rust
// one agent, built on the first fetch: a request-level TLS change bypasses the pool
let tls = TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build();
let config = Config::builder().tls_config(tls).timeout_global(Some(15s)).build();
let agent = Agent::new_with_config(config);
```

`platform-verifier` → `rustls-platform-verifier` uses the Windows platform certificate
store (enterprise roots, constraints, revocation). `win-system-proxy` reads the user's
WinINet proxy settings — correct for a per-user desktop app. (WinHTTP would be for a
Windows *service*; this app is a per-user process.)

**Accept:** the catalog fetch works against a machine with a corporate CA. Note it in
the changelog.

**What is left of the acceptance:** a machine behind a corporate CA. The root store is
asserted by a unit test and the fetch of a public host is in the self-test harness, but
the corporate case itself can only be confirmed by a user on such a machine. The reason
is in `PROGRESS.md`.

**The cost:** the release build grows by **60,416 bytes** (+0.47 %) — the platform
verifier, the WinINet proxy reader and the gzip decoder. `DECISIONS.md` §12 says why that
is accepted and what would pay it back.

### A2. Capability-scoped host API

Introduce the capability table (`DESIGN.md` §2). Because 1.1.3 caches the engine per
thread, scoping is **not** a change to a per-run registration — implement **one engine
per capability set** (`DESIGN.md` §3) so a pack that calls a function it was not
granted fails as an *unknown function*.

**Accept:** a pack that asks for `["files"]` and calls `run_cli` fails with a message
naming the missing capability; the denial is in the pack's log. The 1.1.3 per-run
budget re-arming still holds — a run cannot inherit the previous run's allowance.

### A3. `triggers` + `kind` in the pack schema — **done (2026-10-22)**

`crates/armoury-core/src/pack.rs`. Validate `kind` ∈ {pack, mode}; validate that a
mode pack has at least one trigger; keep `spec` 1 and 2 as they are.

**Accept:** the three shipped packs still load, install, apply, revert and remove
unchanged. A malformed `kind` is a named error, like the other schema errors.

**Done.** The schema carries `kind` (`"pack"` | `"mode"`) and `triggers`, both
additive; a malformed kind is refused by name (`unknown kind 'widget' (expected 'pack'
or 'mode')`). The daemon
does not act on the fields yet — that is A4 and A5. `DECISIONS.md` §18 has the
defaults and the two trigger rules the folder did not state.

### A4. A separate worker queue for modes

The pack worker is a **single serial thread** (`packs.rs:171`,
`while let Ok(job) = jobs.recv()`). A slow mode must never block a pack apply/revert.
Give modes their own thread and channel.

**Accept:** a mode that takes 5 seconds does not delay a pack revert.

### A5. Give the killswitch a config key and an IPC request

It has **neither** today, so it cannot be disabled at all. This is also the natural
place to hang the "is the killswitch module installed" flag.

**Accept:** the killswitch can be turned off in config and by IPC, and the state
survives a restart.

### A6. The one remaining perf decision

`-C target-crt-static` in the release profile. The shipped exe imports
`VCRUNTIME140.dll`, so it is not a truly standalone single file (fine on stock
Win10/11, not on a stripped image). Costs ~1–2 MB. **Decide it explicitly** — it is a
trade-off, not a free win. Record the decision in `DECISIONS.md`.

---

## Phase B — the module catalog

### B1. `kind` in the manifest

`crates/armoury/src/catalog.rs:44` (`CatalogPack`). Manifest schema → 2. Read
`packs` **and** `modules` for one release so an old manifest still works.

**Also update the release workflow** — the "Check the packs against the catalogue" step
in `.github/workflows/release.yml` reads `$catalog.packs`; it must learn the new key
and must keep failing when the release ships bytes the catalogue does not publish.

### B2. Signed manifests

`minisign-verify` 0.3.0. The trust keyring is a file
`crates/armoury/src/trust/keys.json`, embedded with `include_str!`. Verify the
signature **before** parsing the manifest; refuse the whole manifest on a bad
signature; never install anything unverified.

**Accept:** a manifest with a broken signature installs nothing, and says why.

### B3. Install paths per kind

- `pack` / `mode` → the existing `packs::install_text` path (`packs.rs:977`),
  unchanged.
- `helper` → a new path: download → verify sha256 + size → extract into
  `modules/<id>/` → record the resolved path. **The helper's directory needs its own
  ACL** (see `GOTCHAS.md` §7).

**Accept:** a helper installs, is recorded, and is found again after a restart.

### B4. The "Modules" window

`ui.rs:2339` (`install_packs_ui`), `ui.rs:2595` (`catalog_row`). Rename, three row
shapes, the per-module capability display, and the download/verify/install flow behind
the daemon — the UI downloads to a temp file on a worker thread, then sends the
request and the daemon validates and installs.

### B5. Protocol

New `Request` variants in `crates/armoury-core/src/protocol.rs` (the enum runs from
`:9` to `:56`). Keep the existing string-prefix multiplexing pattern (`LAUNCH_ACK`,
`ART_LIST`, `ART_SET`) in mind for anything that is not a first-class response.

---

## Phase C — the console as a helper module

- **C1.** New bin target: `console_ui.rs` + `art.rs` + `pad.rs` become
  `ArmouryConsole.exe`. Add a `[[bin]]` to `crates/armoury/Cargo.toml` (one already
  exists at `:7-9`).
- **C2.** `show_console()` (`daemon.rs:2008`) currently spawns
  `Command::new(current_exe()).arg("--console")`. Spawn the installed helper when one
  is present; fall back to `current_exe() --console` otherwise.
- **C3.** The helper already speaks the existing JSON-lines protocol over the existing
  named pipe and already sends `Request::ConsoleHello`. **No new protocol.**
- **C4.** Decide whether to move the SteamGridDB key out of the elevated daemon
  (`DESIGN.md` §9). Do not half-do it.

---

## Phase D — distribution

- **D1. Inno Setup 6.7.3 installer.** `PrivilegesRequired=lowest`,
  `PrivilegesRequiredOverridesAllowed=dialog`,
  `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`. Ships the **core only — zero packs**.
  The `[Run]` section registers the autostart task. Makes the project
  winget-installable (`InstallerType: inno`).
- **D2. A build-from-source script** (`tools/build-from-source.ps1` + a `.sh` sibling).
  The **auditor / pack-author** channel, not a consumer one:
  - clone the source at a pinned tag and **verify it** (minisign over a release
    tarball, or a signed commit),
  - build with the same `RUSTFLAGS` the project already uses — `-C
    target-cpu=x86-64-v3` from `.cargo/config.toml`. **Not `native`.**
  - copy the exe (+ any selected packs) to a staging directory,
  - **then** delete `target/` and the source. Copy first, verify the exe exists, then
    delete with retry-with-backoff — Defender still holds handles on the fresh
    binaries and the delete fails intermittently. `target/` is 2.85 GB / 1264 files.
  - the "which packs" question belongs at **runtime**, not build time.
- **D3.** Optional: a self-update path for the core exe. A second signed JSON
  (`releases.json`), signed with the **same** minisign key. Prefer ~150 lines of your
  own over `self_update` — it uses a different Ed25519ph scheme, and you want one key
  ecosystem.

---

## Phase E — WASM (deferred, bounded, optional)

**Do not start this before A–D are shipped and stable.** If it is wanted, the prototype
is the killswitch as a WIT component:

- `rustup target add wasm32-wasip2` (Tier 2 on stable since Rust 1.82; a
  `crate-type = ["cdylib"]` crate built for it **emits a component directly**, so
  `cargo-component` and `wasm-tools component new` are not needed for Rust).
- `wit-bindgen` 0.63.0. `wasm-tools` only for core modules from C/C++.
- one WIT interface per capability, semver-qualified (`armoury:windows/hide@1.0.0`).
  A plugin importing an interface you did not wire **fails to instantiate**.
- host: one `wasmtime::component::Linker<T>` per plugin, `T` = the grant set; only
  `func_wrap` the granted interfaces.
- limits map onto the existing ones: `consume_fuel` + `set_fuel` (the 500 000 op cap),
  `epoch_interruption` + `set_epoch_deadline` (the 10 s wall clock),
  `StoreLimitsBuilder::memory_size` (the 64 KiB string cap).
- **Measured cost:** `wasmtime` default features **+5.98 MB** over baseline (reduced
  +4.01 MB); `wasmi` **+1.11 MB** but no Component Model; `Extism` **+8.52 MB** and it
  pins a stale wasmtime 49.
- **Keep Rhai.** WASM is additive, not a replacement.

---

## Acceptance criteria for every phase

- CI green on the branch: `gh run watch`. The self-test harness is part of it.
- The self-test harness is **extended** for the new behaviour.
- The test count in the docs stays honest.
- The daemon's idle cost does not regress: measure it **before and after**, on the
  current tree, and record both numbers in `PROGRESS.md`.
- The exe size does not exceed the current release's size unless the phase is
  explicitly allowed to add it. **Re-measure the baseline** — `strip` changed to
  `"debuginfo"` and the target moved to `x86-64-v3` after the numbers in `GOTCHAS.md`
  were taken. Only Phase E is allowed to add.
- The three shipped packs still install, enable, apply, revert and remove unchanged.
- An existing config with `packs` migrates to `modules` without loss.
- The release workflow's "Check the packs against the catalogue" step still catches a
  mismatch.
- A doc in `docs/` describes the module format, and the packs-repo README
  (`D:\ArmouryDaemon_Packs`) is updated to match.
