# Agent worklog — sync-pipeline rework (2026-09-08)

Source of truth: `AGENTS.md` + user brief (no `docs/toolcraft/*` contract
existed when this started; Toolcraft layers/timeline/renderer decisions are
N/A for this native sync tool — correctly out of scope).

## Diagnosis (verified by reading + live inspection, not guessing)

- Crash = UI-thread freeze, not a Core panic: Push ran snapshot + git
  add/commit/push synchronously on the Slint thread; scans, filter
  keystrokes and auto-tick compared file contents there too.
- Amplifier: open discovery with zero budgets pulled GB-sized cache dirs
  into snapshots and git. Live-caught: 924 MB `pack-objects` from a
  963 MB local object store (149 MB pre-cap blobs) pushed at a fresh
  empty remote — can never succeed (GitHub rejects >100 MB anyway).

## Decisions

1. **Threading:** one `run_job()` helper (busy + label + Send-only crossing,
   panic-proof, timed) + one scan pipeline (`compute_scan` on background,
   `apply_scan` on UI, generation-guarded). `Shared` (Arc + Mutex,
   poison-safe locks) replaced all `Rc<RefCell>` snapshots. `gh auth`
   probes moved off the UI thread (keyring stalls froze the window) into
   a cached + guarded background poll.
2. **Generic model:** preset table deleted. Every `~/.config` entry is an
   app (`rel_paths == [id]`); only `FAVORITES` pin order + global
   `MACHINE_LOCAL` excludes remain. `target_for_rel` collapsed to one line.
   Known migration note: legacy `omarchy-shell/` repo dirs become orphaned
   (never deleted); re-push from the `omarchy` dir.
3. **Budgets:** 25 MB/file + 20k files/app caps with honest `SkipStats`
   (surfaced in logs); giants compare by size only (documented heuristic);
   own `.bak.*` litter never syncs.
4. **Errors:** typed `ErrorKind` (Auth/Network/Conflict/NotRepo/
   MissingRemote), `classify()` on stderr, hint in `Display`. Divergence
   aborts strictly (no force-push, no silent skip); auth aborts
   (actionable); transient network stays a skipped note. `MissingRemote`
   is auto-created by Push, then pushed once.
5. **No hanging children:** every child runs bounded (90 s net / 15 s
   local) in its own process group; expiry kills the whole tree
   (`libc::killpg`) — orphans like pack-objects/ssh impossible.
6. **Push gates + repair:** `check_pushable` (two-tier: ms count-objects
   pre-check, exact batch-check only for big stores) aborts unpushable
   history with a fresh-start recipe; `ensure_upstream` repairs missing
   tracking; empty remotes report cleanly; Setup attaches already-existing
   remotes instead of demanding pasted URLs.
7. **Structure:** `core/src/git.rs` (1571 lines) split into
   `git/{mod,run,ops,auth,ssh}.rs` — mechanical, symbol-preserving
   (87/87 symbols verified), zero behavior change.

## Verification tier (per phase)

`cargo fmt --all --check`, `cargo test` (42 core tests incl. caps,
classify, generic discovery, kill, upstream, timeouts), `cargo clippy
--all-targets -- -Dwarnings`, `git diff --check`, Slint compile via
`cargo check -p ocs_gui`, detector clean. No GUI test harness exists
(headless Slint backend unused) — manual click-through (login code,
Use SSH, cancel, fresh-setup recovery) owed by the user.
