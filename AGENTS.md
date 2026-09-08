# AGENTS.md — omarchy-config-sync (fresh restart)

## Idea

Super lightweight, small, efficient per-app config manager for Omarchy Linux (2 machines).

- Fully generic: every top-level `~/.config` entry is an app, no per-app
  definitions. Favorites pinned on top, machine-local basenames excluded
  globally (see App catalog).
- Scanner lists `~/.config/<app>` state (exists / missing / size).
- `Push`: copy app config -> local clone of private data-repo -> `git push` (system git + SSH, no tokens in app).
  Missing remote repo on first push is created automatically (`gh repo create --private`), then pushed once.
  Histories holding >100 MB blobs (pre-cap era) abort fast with a fresh-start recipe — GitHub would reject them anyway.
- `Pull/Apply`: fetch data-repo -> preview diff -> backup `~/.config/<app>` to `~/.config/<app>.bak.<timestamp>` -> copy in.
- Two repos: this repo = public app; second private repo = only synced configs (`<app>/...` + `manifest.json`).
- Budgets instead of surprises: files over 25 MB and tails past 20k files
  per app are skipped + counted, never dragged along. All blocking work
  (scan, snapshot, git) runs on background threads — the UI never freezes.

Old Rise/OWM codebase is deleted on purpose. Keep only the spirit: Rust workspace, `core` owns logic, Slint GUI is thin.

## Layout (minimal, no bloat)

```text
Cargo.toml        workspace [core, gui]
core/             lib: scanner (parallel, XDG), apps (generic discovery,
                  favorites pinned, global machine-local excludes),
                  store (compare with budgets + skip stats, selective
                  snapshot/apply with merge-back, restore, repo listing),
                  git (clone/init/fetch/pull/push/status, SSH, typed errors),
                  theme (live Omarchy colors + rounding + fingerprint),
                  prefs (don't-show-again), clipboard (copy log)
gui/              Slint app, thin: sidebar (favorites + all, checkboxes),
                  per-file checklist with sync states, upload/download modes,
                  repo panel (autonomous init, clone, updates, Use SSH),
                  confirm dialog, restore picker, terminal log with copy.
                  Jobs (`run_job`) + one scan pipeline (`compute/apply_scan`,
                  generation-guarded) keep the UI thread render-only.
                  Custom Field/Icon/Check components (theme radius/accent,
                  Nerd Font). No theme UI. No keybinds.
```

No `workstation/`, no themes, no package manager, no provisioning engine, no docs site, no TUI.

## Foundation to keep

- Rust 2021 workspace, `core` = all logic, `gui` = render/input/confirm only.
- Slint 1.17 for GUI (own package in workspace, `slint-build`).
- `cargo fmt`, `cargo test`, `cargo clippy -- -Dwarnings`, `git diff --check`.
- One shared preview plan for pull/apply; manifest records only what succeeded.

## SAFETY — critical (old app wiped a PC)

1. NEVER delete user configs. Copy-only. No `rm -rf` on `~/.config`, no `git clean -fd`, no destructive `git checkout --force` on HOME.
2. Apply path is ONLY: preview -> confirm -> backup (`<target>.bak.<epoch>`) -> copy file-by-file. If backup fails, abort.
3. NEVER target machine-local files: `monitors.lua`, `input.lua` stay local
   everywhere via the global `MACHINE_LOCAL` excludes (no per-app presets).
4. NEVER force-push, NEVER auto-merge divergence: a diverged data-repo aborts
   Sync with a Conflict error, the user resolves, then syncs again.
5. NEVER edit `/usr/share/omarchy/` (read-only reference).
6. NEVER run apply against real `$HOME` in tests or validation. Tests use `tempfile` temp dirs only. GUI validation uses preview or temp HOME.
7. Git ops run only inside the data-repo clone, never in `$HOME`. SSH only, no PAT storage. Git never prompts: stdin is null, `GIT_TERMINAL_PROMPT=0`, failures are typed errors.
   No child may hang a job: network calls time out after 90 s (local 15 s),
   expiry kills the child and names the command. SSH runs batch-mode unless
   the user set `GIT_SSH_COMMAND`; non-interactive `gh` runs prompt-disabled.
8. `Apply, then diff again` must be clean; partial runs never pose as success.
9. Scanner is read-only + parallel (one thread per app), resolves `$XDG_CONFIG_HOME`
   else `~/.config`, never follows symlinks, counts skipped/broken instead of crashing.
10. Restore copies a `.bak.*` back file-by-file and never deletes; files missing from
    the backup stay untouched and are reported as leftovers.
11. Theme (colors + radius + selection) is read live from the active Omarchy
    theme and re-applied on change (fingerprint poll). No theme UI in the app.

## App catalog (generic discovery)

- Every top-level `~/.config` entry is an app owning exactly that entry.
  No registry, no per-app definitions — cloned repos and new machines work
  with zero configuration.
- Favorites pinned on top: zed, hypr, omarchy-shell, kitty, fastfetch, nvim.
  Missing favorites show "not installed", never error.
- Machine-local basenames (`monitors.lua`, `input.lua`) are excluded globally.
- Upload mode: tick local files -> Push (top button). Download mode (after
  Clone): tick repo apps -> Apply to device (backup + full apply,
  local-only extras → backup). Sync only syncs the repo clone and never
  touches `~/.config`.

## Validation

```bash
cargo fmt --all --check
cargo test
cargo clippy --all-targets -- -Dwarnings
git diff --check
```

GUI headless: Slint testing backend, no display needed. Never `cargo run` apply against real home.
