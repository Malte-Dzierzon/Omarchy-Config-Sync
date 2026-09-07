# AGENTS.md — omarchy-config-sync (fresh restart)

## Idea

Super lightweight, small, efficient per-app config manager for Omarchy Linux (2 machines).

- User picks favorite apps only (v1: `zed`, `hypr`, `omarchy-shell`).
- Scanner lists `~/.config/<app>` state (exists / missing / size).
- `Push`: copy app config -> local clone of private data-repo -> `git push` (system git + SSH, no tokens in app).
- `Pull/Apply`: fetch data-repo -> preview diff -> backup `~/.config/<app>` to `~/.config/<app>.bak.<timestamp>` -> copy in.
- Two repos: this repo = public app; second private repo = only synced configs (`<app>/...` + `manifest.json`).

Old Rise/OWM codebase is deleted on purpose. Keep only the spirit: Rust workspace, `core` owns logic, Slint GUI is thin.

## Layout (minimal, no bloat)

```text
Cargo.toml        workspace [core, gui]
core/             lib: scanner (parallel, XDG), apps (presets + live discovery,
                  favorites pinned), store (compare, selective snapshot/apply
                  with merge-back, restore, repo listing), git (clone/init/
                  fetch/pull/push/status, SSH), theme (live Omarchy colors +
                  rounding + fingerprint)
gui/              Slint app, thin: sidebar (favorites + all, checkboxes),
                  per-file checklist with sync states, upload/download modes,
                  repo panel (autonomous init, clone, updates), terminal log.
                  Custom Field/Icon/Check components (theme radius/accent,
                  Nerd Font). No theme UI.
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
3. NEVER target machine-local files: `monitors.lua`, `input.lua`, `hyprsunset.conf` schedules stay local by default (excluded in `hypr` preset).
4. NEVER edit `/usr/share/omarchy/` (read-only reference).
5. NEVER run apply against real `$HOME` in tests or validation. Tests use `tempfile` temp dirs only. GUI validation uses preview or temp HOME.
6. Git ops run only inside the data-repo clone, never in `$HOME`. SSH only, no PAT storage.
7. `Apply, then diff again` must be clean; partial runs never pose as success.
8. Scanner is read-only + parallel (one thread per app), resolves `$XDG_CONFIG_HOME`
   else `~/.config`, never follows symlinks, counts skipped/broken instead of crashing.
9. Restore copies a `.bak.*` back file-by-file and never deletes; files missing from
   the backup stay untouched and are reported as leftovers.
10. Theme (colors + radius + selection) is read live from the active Omarchy
    theme and re-applied on change (fingerprint poll). No theme UI in the app.

## App catalog (presets + live discovery)

- Presets (with excludes): `zed`, `hypr` (MINUS `monitors.lua`, `input.lua`),
  `omarchy-shell` (`shell.json` + `extensions/`), `alacritty`, `ghostty`,
  `foot`, `kitty`, `starship` (single file), `btop`, `lazygit`, `fastfetch`,
  `nvim`
- Favorites pinned on top: zed, hypr, omarchy-shell, kitty, fastfetch, nvim.
  Everything else in `~/.config` is discovered automatically (custom apps
  need no registration). Missing apps show "not installed", never error.
- Upload mode: tick local files -> Push. Download mode (after Clone): tick
  repo apps -> Übernehmen (backup + full apply, local-only extras → backup).

## Validation

```bash
cargo fmt --all --check
cargo test
cargo clippy --all-targets -- -Dwarnings
git diff --check
```

GUI headless: Slint testing backend, no display needed. Never `cargo run` apply against real home.
