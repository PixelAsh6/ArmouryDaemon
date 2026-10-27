# Writing a pack

A pack teaches the daemon how to move one application between two states: the state the
machine belongs in while a tracked app runs, and the state it belongs in otherwise. Everything
the daemon does to another program is a pack, so a game launcher, a fan-control tool or a
monitor layout switcher is one folder with a `pack.json` in it.

This page is for someone who wants to write one and publish it. The rest of the design is in
[modularity/DESIGN.md](modularity/DESIGN.md); the decisions are in
[modularity/DECISIONS.md](modularity/DECISIONS.md).

## One pack, one folder

```
packs/g-helper/pack.json      ← the shipped pack, and the example to copy
packs/rainmeter/pack.json
packs/wallpaper-engine/pack.json
```

The folder name is the pack's `id`. Nothing else is required: a pack with no script and no
assets is that one file. A pack that needs a script puts an `.rhai` file beside it and names it
in the pack.

A pack is installed on its own. The Packs window downloads one entry, verifies its SHA-256
against the catalogue, and the daemon writes it into its own folder — nothing else is touched,
and a pack that fails to install leaves the others alone. A pack the user never installs costs
them nothing: no download, no disk, no permissions.

## The file

```json
{
  "spec": 1,
  "id": "g-helper",
  "name": "G-Helper",
  "version": "1.3.1",
  "order": 20,
  "targets": { "process": ["GHelper.exe"] },
  "settings": [
    {
      "key": "mode",
      "label": "Mode while a tracked app runs",
      "type": "enum",
      "values": ["Turbo", "Balanced", "Silent"],
      "default": "Turbo"
    }
  ],
  "capture": [
    {
      "key": "prev_mode",
      "mechanism": "json_file",
      "path": "%APPDATA%\\GHelper\\config.json",
      "pointer": "/performance_mode"
    },
    { "key": "was_running", "mechanism": "process_running" }
  ],
  "apply": {
    "guard": { "targetRunning": true },
    "steps": [
      {
        "mechanism": "hotkey",
        "keys_from_mode": "{{settings.mode}}",
        "map": {
          "Turbo": "ctrl+shift+alt+f18",
          "Balanced": "ctrl+shift+alt+f17",
          "Silent": "ctrl+shift+alt+f16"
        }
      }
    ]
  },
  "revert": {
    "guard": { "targetRunning": true },
    "steps": [
      { "mechanism": "hotkey", "keys_from_captured": "prev_mode" }
    ]
  }
}
```

What the keys mean:

- **`spec`** — `1`. The format is versioned; a pack the daemon does not understand is refused,
  not guessed at.
- **`id`** — lowercase, hyphens. It is the folder name, the catalogue key and what the settings
  window shows in a log line, so it must not change once published.
- **`name`, `version`** — what the window shows. `version` is what "a newer catalogue entry"
  is compared against, so bump it for every release.
- **`order`** — packs apply in ascending order and revert in descending order. A pack that
  stops something another pack needs goes later.
- **`targets.process`** — the exe file names the pack belongs to. These are the names the
  daemon's own watcher reports, so `GHelper.exe`, not a full path.
- **`settings`** — the dropdown the dashboard shows for this pack. `type` is `enum`; the
  dropdown's value is available to the steps as `{{settings.<key>}}`.
- **`capture`** — what the pack remembers before it changes anything, so the revert can put it
  back. `process_running` records whether the target was up; `json_file` reads one pointer
  (`/performance_mode`) out of a JSON file and `hotkey` can replay it with
  `keys_from_captured`.
- **`apply` / `revert`** — each a `guard` plus `steps`. The guard says when the block may run
  (`targetRunning`: the app is up / down); the steps are what it does. Steps are `cli` (run an
  executable with `args`, optionally waiting) and `hotkey` (send a key combination to the
  target or to the focused window). `{{target.name}}`, `{{settings.<key>}}` and
  `{{captured.<key>}}` are substituted into any string.
- **`capabilities`** (optional) — a pack that carries a script names what the script may touch.
  The daemon builds an interpreter that carries **only** the granted functions, so an ungranted
  call cannot exist at runtime. A pack asks; the user grants.

## A script instead of steps

A pack whose logic does not fit `apply`/`revert` steps may carry a small Rhai script:

```json
"script": { "apply": "apply.rhai", "revert": "revert.rhai" },
"capabilities": ["process", "window"]
```

The script runs in the daemon's sandboxed interpreter with only the functions its capabilities
grant. It may not spawn processes, read files or touch the registry unless the capability for
that is granted.

## Publishing it

1. **Fork `PixelAsh6/ArmouryDaemon_Packs`** and add `packs/<id>/pack.json` (and any script or
   asset it names).
2. **Add the entry** to `catalog/index.json` (schema 2). Each entry carries `id`, `name`,
   `version`, `kind` (`"pack"`), `description`, `file` (the path of `pack.json` in the repo),
   `sha256` and `size` of that file.
3. **Open a pull request.** That is the approval step: the catalogue is the list the app trusts,
   so a pack nobody approved is a pack nobody can install.

What a review looks at, in order:

- The `apply`/`revert` pair is symmetric: whatever `apply` changes, `revert` puts back, using
  the capture.
- Nothing writes outside the pack's own target: a pack that rewrites a game's files or a
  registry key it did not capture is refused.
- The guard is right. A pack that toggles a fan curve must not run its revert while the app is
  up.
- `id`, `version`, `sha256` and `size` agree with the file.

The main catalogue is **signed**. The keeper signs `catalog/index.json` with the private key;
the daemon carries the public keyring and verifies the signature before it parses a single
entry. So approving a pack means merging the pull request, and publishing it means one more
signature by the keeper — a pack cannot be slipped into the app behind the signature, and the
private key never enters a repository.

## Testing a pack before you publish

- **Locally:** install it with the Packs window's **Local pack** button — it picks a
  `pack.json` from disk and validates it before installing, so a broken file lands nowhere.
- **The catalogue path:** point the daemon at your own catalogue (`catalog_url` in the config)
  and install the entry from the window; that is the whole path a real user takes, minus the
  signature.
- **Both directions:** the dashboard shows whether the packs are applied. Start the tracked app
  and watch the change, close it and watch the revert, and check the app's own UI to see that
  the state really moved.
