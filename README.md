# omarchy-config-sync

Lightweight per-app config sync for Omarchy (`~/.config` -> private git repo -> other machine).

- Pick favorite apps only (v1: `zed`, `hypr`, `omarchy-shell`).
- Push/pull over system `git` + SSH. No tokens in the app.
- Apply always backs up to `~/.config/<app>.bak.<epoch>` first. Copy-only, never delete.

See `AGENTS.md` for architecture + safety rules.

```bash
cargo test
cargo run -p ocs_gui   # needs a display; Scan is read-only
```

## Fast iteration (Slint is heavy, core is light)

```bash
cargo test -p ocs_core        # seconds, no GUI build at all
cargo check -p ocs_gui        # typecheck UI without codegen
cargo run -p ocs_gui          # full build only when you run it
```

Dev profile uses `debug = "line-tables-only"` (small `target/`, fast link,
still useful panics). Full debuginfo only if you debug inside dependencies.
`target/` is disposable: `cargo clean` wipes it, next build rewarms in ~1 min.
