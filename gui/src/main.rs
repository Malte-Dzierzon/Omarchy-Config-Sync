//! Thin Slint GUI over ocs_core. Wiring + log output only.
//! The Omarchy theme (colors + radius) is applied silently at startup and
//! re-applied live when it changes (polled fingerprint, no theme UI).
//! Safety: Scan/compare are read-only. Push writes only into the repo clone.
//! Apply previews first, backs up to `<target>.bak.<epoch>`, merges unselected
//! files back, then copies. Restore never deletes.

slint::include_modules!();

use ocs_core::{git, scanner, store, theme};
use slint::{Color, Model, ModelRc, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, MutexGuard,
};

/// Shorthand maps kept in [`Shared`].
type CheckMap = HashMap<String, HashSet<PathBuf>>;
type SideMap = HashMap<String, (bool, bool)>;

/// UI-independent state shared between the UI thread and background jobs.
/// Everything here is Send: jobs read/write through short locks instead of
/// snapshotting `Rc<RefCell>`s around, so follow-up work can always be
/// scheduled — even from inside a completion handler on the UI thread.
#[derive(Clone, Default)]
struct Shared {
    /// Per-app remembered file checks (upload checklist).
    checks: Arc<Mutex<CheckMap>>,
    /// Sidebar checks + active flags (survives filters and rescans).
    app_state: Arc<Mutex<SideMap>>,
    /// Pending destructive op behind the confirm dialog.
    pending: Arc<Mutex<Option<PendingOp>>>,
    /// Scan generation: stale background results drop themselves.
    seq: Arc<AtomicU64>,
    /// Cached `gh` auth: `gh` may touch the system keyring (stalls!), so the
    /// UI thread NEVER probes — it reads this (≤30 s stale, fine for status).
    auth: Arc<Mutex<git::GhAuth>>,
    /// One auth probe at a time; concurrent ticks skip.
    auth_busy: Arc<AtomicBool>,
    /// Device-login cancel flag (polled by the `gh` waiter).
    login_cancel: Arc<AtomicBool>,
    /// SSH probe generation: superseded probes stay silent.
    ssh_gen: Arc<AtomicU64>,
}

/// Lock without poison panic: a crashed worker must never take the UI down.
fn lock<'a, T>(m: &'a Mutex<T>) -> MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Outcome of a background job: regular result plus wall time — or a caught
/// panic payload. A panicking worker surfaces in the log and always clears
/// `busy`; it can never strand the UI in silence.
pub struct JobResult<T> {
    pub out: Result<T, String>,
    pub elapsed: std::time::Duration,
}

/// Short duration for log lines ("0.4s", "12.3s").
fn secs(d: std::time::Duration) -> String {
    format!("{:.1}s", d.as_secs_f32())
}

impl<T> JobResult<T> {
    /// Unwrap the panic layer: `None` (plus a visible log line) only when
    /// the worker itself panicked. Regular work results pass through.
    fn value(self, ui: &AppWindow, what: &str) -> Option<T> {
        match self.out {
            Ok(v) => Some(v),
            Err(p) => {
                ui.set_log_text(
                    format!(
                        "{what} crashed the worker (bug, please report): {p}\nNothing was applied."
                    )
                    .into(),
                );
                None
            }
        }
    }

    /// Uniform timing tail for completion lines (" · 0.4s").
    fn took(&self) -> String {
        format!(" · {}", secs(self.elapsed))
    }
}

/// Run blocking work in the background with busy state + label; `apply`
/// finishes on the UI thread. Only Send data crosses threads. Starting a job
/// invalidates in-flight scans so their stale results drop themselves.
/// The click logs synchronously first, so silence is impossible: either a
/// "started" line or a busy/confirm reason is always visible.
fn run_job<T: Send + 'static>(
    ui: &AppWindow,
    shared: &Shared,
    label: &str,
    work: impl FnOnce() -> T + Send + 'static,
    apply: impl FnOnce(&AppWindow, JobResult<T>) + Send + 'static,
) {
    if ui.get_busy() {
        ui.set_log_text("busy — wait a moment".into());
        return;
    }
    if ui.get_show_confirm() {
        return;
    }
    shared.seq.fetch_add(1, Ordering::SeqCst);
    ui.set_busy(true);
    ui.set_sync_label(label.into());
    ui.set_log_text(format!("▸ {label}…").into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).map_err(|p| {
            p.downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string())
        });
        let res = JobResult {
            out,
            elapsed: t0.elapsed(),
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_busy(false);
                ui.set_sync_label("Sync".into());
                apply(&ui, res);
            }
        });
    });
}

/// Owned scan input: everything the background thread needs (all Send).
/// Built on the UI thread by [`start_scan`].
struct ScanInput {
    cfg: PathBuf,
    repo: PathBuf,
    keep: SideMap,
    needle: String,
    download_mode: bool,
    active_app: String,
    checks: CheckMap,
    repo_keep: HashSet<String>,
    fetch_first: bool,
    write_log: bool,
}

struct RowData {
    id: String,
    name: String,
    checked: bool,
    active: bool,
    sub: String,
    state: i32,
}

struct FileRow {
    path: String,
    checked: bool,
    note: String,
    state: i32,
}

/// One background pass over everything the main view shows. Each selected app
/// is compared ONCE and the result feeds dots, file list and summary alike
/// (the old pipeline compared twice: sidebar refresh + file list rebuild).
struct ScanData {
    favs: Vec<RowData>,
    rest: Vec<RowData>,
    fav_header: String,
    other_header: String,
    files: Vec<FileRow>,
    repo_files: Vec<FileRow>,
    sync_summary: String,
    log: Option<String>,
    backups: Vec<(String, String)>,
    has_selection: bool,
    has_repo_selection: bool,
    app_detail: String,
    push_label: String,
    active_title: String,
}

fn row_data(
    a: &ocs_core::AppSpec,
    checked: bool,
    active: bool,
    sub: String,
    state: i32,
) -> RowData {
    RowData {
        id: a.id.clone(),
        name: a.label.clone(),
        checked,
        active,
        sub,
        state,
    }
}

fn compute_scan(inp: &ScanInput) -> ScanData {
    if inp.fetch_first {
        let _ = git::fetch(&inp.repo); // quiet; status line refreshes on arrival
    }
    let needle = inp.needle.to_lowercase();
    let mut apps = Vec::new();
    for a in ocs_core::apps::discover_apps(&inp.cfg) {
        if !needle.is_empty()
            && !a.label.to_lowercase().contains(&needle)
            && !a.id.to_lowercase().contains(&needle)
        {
            continue;
        }
        apps.push(a);
    }
    let mut cmp_cache: HashMap<String, Vec<store::FileCompare>> = HashMap::new();
    let mut skipped = store::SkipStats::default();
    let mut synced = 0usize;
    let mut changed = 0usize;
    let mut sel_count = 0usize;
    let mut favs = Vec::new();
    let mut rest = Vec::new();
    for a in &apps {
        let (checked, active) = inp
            .keep
            .get(&a.id)
            .copied()
            .unwrap_or((a.is_favorite(), a.id == "zed"));
        let row = if !checked {
            row_data(a, checked, active, "skipped".to_string(), 0)
        } else {
            sel_count += 1;
            let installed = a.rel_paths.iter().any(|r| inp.cfg.join(r).exists());
            if !installed {
                row_data(a, checked, active, "not installed".to_string(), 0)
            } else {
                let (cmp, st) =
                    store::compare(&inp.cfg, &inp.repo.join(&a.id), a, None).unwrap_or_default();
                skipped.add(&st);
                if cmp.is_empty() {
                    row_data(a, checked, active, "no files".to_string(), 0)
                } else {
                    let mut c = 0usize;
                    for f in &cmp {
                        match f.state {
                            store::FileState::Synced => synced += 1,
                            _ => {
                                c += 1;
                                changed += 1;
                            }
                        }
                    }
                    let (sub, state) = if c == 0 {
                        (format!("{} files · synced", cmp.len()), 1)
                    } else {
                        (format!("{} files · {c} changed", cmp.len()), 2)
                    };
                    let row = row_data(a, checked, active, sub, state);
                    cmp_cache.insert(a.id.clone(), cmp);
                    row
                }
            }
        };
        if a.is_favorite() {
            favs.push(row);
        } else {
            rest.push(row);
        }
    }
    // Active app file list, reused from the cache when selected (old show_app
    // compared on demand even for unchecked apps — same here).
    let active = ocs_core::apps::resolve_app(&inp.cfg, &inp.active_app);
    if !cmp_cache.contains_key(&active.id) {
        let (cmp, st) =
            store::compare(&inp.cfg, &inp.repo.join(&active.id), &active, None).unwrap_or_default();
        skipped.add(&st);
        cmp_cache.insert(active.id.clone(), cmp);
    }
    let cmp = &cmp_cache[&active.id];
    let remembered = inp.checks.get(&inp.active_app);
    let mut chosen = 0usize;
    let mut push_n = 0usize;
    let files: Vec<FileRow> = cmp
        .iter()
        .map(|c| {
            let checked = remembered.map(|s| s.contains(&c.rel)).unwrap_or(true);
            let st = state_idx(c.state);
            if checked {
                chosen += 1;
                if st == 1 || st == 2 {
                    push_n += 1;
                }
            }
            FileRow {
                path: c.rel.to_string_lossy().into_owned(),
                checked,
                note: format!("{} · {}", human_bytes(c.bytes), state_word(c.state)),
                state: st,
            }
        })
        .collect();
    // Download picker rows (compare reused per repo app).
    let mut repo_files = Vec::new();
    if inp.download_mode {
        for ra in store::list_repo_apps(&inp.repo) {
            let app = ocs_core::apps::resolve_app(&inp.cfg, &ra.id);
            let (cmp, st) =
                store::compare(&inp.cfg, &inp.repo.join(&app.id), &app, None).unwrap_or_default();
            skipped.add(&st);
            let synced_n = cmp
                .iter()
                .filter(|c| c.state == store::FileState::Synced)
                .count();
            let dirty = synced_n != cmp.len();
            let checked = if inp.repo_keep.is_empty() {
                dirty
            } else {
                inp.repo_keep.contains(&ra.id)
            };
            repo_files.push(FileRow {
                path: ra.id.clone(),
                checked,
                note: format!(
                    "{} files · {} · {synced_n}/{} synced",
                    ra.files,
                    human_bytes(ra.bytes),
                    cmp.len()
                ),
                state: if dirty { 1 } else { 0 },
            });
        }
    }
    // Backup picker entries, newest first (same walk as the old hint).
    let mut backups: Vec<(String, String)> = Vec::new();
    for a in ocs_core::apps::discover_apps(&inp.cfg) {
        for b in store::list_backups(&inp.cfg, &a) {
            if let Some(n) = b.backup.file_name().and_then(|s| s.to_str()) {
                backups.push((n.to_string(), a.label.clone()));
            }
        }
    }
    backups.sort_by(|x, y| y.0.cmp(&x.0));
    backups.truncate(20);
    let sync_summary = if sel_count == 0 {
        "no apps selected".to_string()
    } else {
        format!("{synced} synced · {changed} changed")
    };
    let has_repo_selection = repo_files.iter().any(|r| r.checked);
    let log = inp.write_log.then(|| {
        let mut s = String::from("scan (read-only):\n");
        for r in favs.iter().chain(rest.iter()) {
            if r.checked {
                s.push_str(&format!("- {}: {}\n", r.id, r.sub));
            }
        }
        if skipped.total() > 0 || skipped.truncated {
            s.push_str(&format!("skipped{}\n", skip_suffix(&skipped)));
        }
        s
    });
    ScanData {
        fav_header: if inp.needle.trim().is_empty() {
            "FAVORITES".to_string()
        } else {
            format!("FAVORITES ({})", favs.len())
        },
        other_header: format!("ALL APPS ({})", rest.len()),
        favs,
        rest,
        files,
        repo_files,
        sync_summary,
        log,
        backups,
        has_selection: chosen > 0,
        has_repo_selection,
        app_detail: format!("{chosen} of {} checked", cmp.len()),
        push_label: if push_n == 0 {
            "Push".to_string()
        } else {
            format!("Push · {push_n}")
        },
        active_title: app_title(&inp.active_app),
    }
}

/// Apply a finished [`ScanData`] on the UI thread (Slint types are built
/// here, never across threads).
fn apply_scan(ui: &AppWindow, d: ScanData) {
    let rows = |v: Vec<RowData>| {
        ModelRc::new(VecModel::from(
            v.into_iter()
                .map(|r| AppEntry {
                    id: r.id.into(),
                    name: r.name.into(),
                    checked: r.checked,
                    active: r.active,
                    sub: r.sub.into(),
                    state: r.state,
                })
                .collect::<Vec<_>>(),
        ))
    };
    let frows = |v: Vec<FileRow>| {
        ModelRc::new(VecModel::from(
            v.into_iter()
                .map(|r| FileEntry {
                    path: r.path.into(),
                    checked: r.checked,
                    note: r.note.into(),
                    state: r.state,
                })
                .collect::<Vec<_>>(),
        ))
    };
    ui.set_fav_header(d.fav_header.into());
    ui.set_other_header(d.other_header.into());
    ui.set_fav_apps(rows(d.favs));
    ui.set_other_apps(rows(d.rest));
    ui.set_files(frows(d.files));
    ui.set_repo_files(frows(d.repo_files));
    ui.set_backup_items(ModelRc::new(VecModel::from(
        d.backups
            .into_iter()
            .map(|(n, label)| FileEntry {
                path: n.into(),
                checked: false,
                note: label.into(),
                state: 0,
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_sync_summary(d.sync_summary.into());
    ui.set_has_selection(d.has_selection);
    ui.set_has_repo_selection(d.has_repo_selection);
    ui.set_app_detail(d.app_detail.into());
    ui.set_push_label(d.push_label.into());
    ui.set_app_title(d.active_title.into());
    if let Some(log) = d.log {
        ui.set_copy_label("Copy".into());
        ui.set_log_text(log.into());
    }
}

/// Snapshot UI state on this thread, compute in the background, apply if
/// still current (generation guard drops superseded results, e.g. while
/// typing in the filter).
fn start_scan(ui: &AppWindow, shared: &Shared, fetch_first: bool, write_log: bool) {
    if ui.get_busy() {
        return; // a mutating job runs; its follow-up scan covers us
    }
    persist_sidebar(ui, &shared.app_state);
    let input = ScanInput {
        cfg: expand(&ui.get_config_dir()),
        repo: expand(&ui.get_repo_dir()),
        keep: lock(&shared.app_state).clone(),
        needle: ui.get_app_filter().to_string(),
        download_mode: ui.get_download_mode(),
        active_app: ui.get_active_app().to_string(),
        checks: lock(&shared.checks).clone(),
        repo_keep: read_repo_checks(ui),
        fetch_first,
        write_log,
    };
    let my = shared.seq.fetch_add(1, Ordering::SeqCst) + 1;
    let weak = ui.as_weak();
    let seq = shared.seq.clone();
    std::thread::spawn(move || {
        let data = compute_scan(&input);
        let _ = slint::invoke_from_event_loop(move || {
            if seq.load(Ordering::SeqCst) != my {
                return; // superseded by a newer scan
            }
            if let Some(ui) = weak.upgrade() {
                apply_scan(&ui, data);
            }
        });
    });
}

fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

fn rgb(c: theme::Rgb) -> Color {
    Color::from_rgb_u8(c.r, c.g, c.b)
}

fn human_bytes(b: u64) -> String {
    if b < 1024 {
        format!("{b} B")
    } else if b < 1024 * 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{:.1} MB", b as f64 / 1024.0 / 1024.0)
    }
}

/// ", 2 large skipped" log suffix for skip stats, or "" when clean.
fn skip_suffix(s: &store::SkipStats) -> String {
    if s.total() == 0 && !s.truncated {
        return String::new();
    }
    let mut parts = Vec::new();
    if s.large > 0 {
        parts.push(format!("{} large", s.large));
    }
    if s.unreadable > 0 {
        parts.push(format!("{} unreadable", s.unreadable));
    }
    if s.truncated {
        parts.push(format!("truncated at {}", store::MAX_FILES_PER_APP));
    }
    format!(", skipped {}", parts.join(", "))
}

fn apply_theme(ui: &AppWindow, t: &theme::OmarchyTheme) {
    ui.set_bg(rgb(t.background));
    ui.set_surface(rgb(t.surface));
    ui.set_sel_bg(rgb(t.selection));
    ui.set_console(rgb(t.surface_dark));
    ui.set_ink(rgb(t.foreground));
    ui.set_faint(rgb(t.muted));
    ui.set_accent(rgb(t.accent));
    let lum = 0.299 * f32::from(t.accent.r)
        + 0.587 * f32::from(t.accent.g)
        + 0.114 * f32::from(t.accent.b);
    ui.set_on_accent(if lum > 140.0 {
        rgb(t.surface_dark)
    } else {
        rgb(t.foreground)
    });
    ui.set_ok(rgb(t.success));
    ui.set_warn(rgb(t.warning));
    ui.set_bad(rgb(t.danger));
    ui.set_radius(t.radius);
}

fn state_word(s: store::FileState) -> &'static str {
    match s {
        store::FileState::Synced => "synced",
        store::FileState::Modified => "modified",
        store::FileState::NewLocal => "new",
        store::FileState::MissingLocal => "in repo only",
    }
}

fn state_idx(s: store::FileState) -> i32 {
    match s {
        store::FileState::Synced => 0,
        store::FileState::Modified => 1,
        store::FileState::NewLocal => 2,
        store::FileState::MissingLocal => 3,
    }
}

fn app_title(id: &str) -> String {
    // Generic display name: no per-app table (prettify is the single rule).
    ocs_core::apps::resolve_app(Path::new(""), id).label
}

fn snapshot_sidebar(ui: &AppWindow) -> HashMap<String, (bool, bool)> {
    let mut map = HashMap::new();
    for m in [ui.get_fav_apps(), ui.get_other_apps()] {
        for i in 0..m.row_count() {
            if let Some(r) = m.row_data(i) {
                map.insert(r.id.to_string(), (r.checked, r.active));
            }
        }
    }
    map
}

/// Merge the visible rows into the persistent sidebar state, so filtered-out
/// apps keep their checks/active flag across rescans and filter changes.
fn persist_sidebar(ui: &AppWindow, state: &Mutex<SideMap>) {
    lock(state).extend(snapshot_sidebar(ui));
}

fn for_each_app_row(ui: &AppWindow, mut f: impl FnMut(&ModelRc<AppEntry>, usize, AppEntry)) {
    for m in [ui.get_fav_apps(), ui.get_other_apps()] {
        for i in 0..m.row_count() {
            if let Some(r) = m.row_data(i) {
                f(&m, i, r);
            }
        }
    }
}

fn selected_ids(ui: &AppWindow) -> Vec<String> {
    let mut v = Vec::new();
    for_each_app_row(ui, |_, _, r| {
        if r.checked {
            v.push(r.id.to_string());
        }
    });
    v
}

fn config_and_repo(ui: &AppWindow) -> (PathBuf, PathBuf) {
    (expand(&ui.get_config_dir()), expand(&ui.get_repo_dir()))
}

/// Checked repo-layout rels currently shown in the file list.
fn read_checks(ui: &AppWindow) -> HashSet<PathBuf> {
    let m = ui.get_files();
    let mut set = HashSet::new();
    for i in 0..m.row_count() {
        if let Some(r) = m.row_data(i) {
            if r.checked {
                set.insert(PathBuf::from(r.path.as_str()));
            }
        }
    }
    set
}

fn set_all_checks(ui: &AppWindow, v: bool) {
    let m = ui.get_files();
    for i in 0..m.row_count() {
        if let Some(mut r) = m.row_data(i) {
            r.checked = v;
            m.set_row_data(i, r);
        }
    }
}

/// Remember the visible checklist for the active app.
fn remember(ui: &AppWindow, checks: &Mutex<CheckMap>) {
    lock(checks).insert(ui.get_active_app().to_string(), read_checks(ui));
}

/// Checked app ids in the download picker (repo-files model).
fn read_repo_checks(ui: &AppWindow) -> HashSet<String> {
    let m = ui.get_repo_files();
    let mut set = HashSet::new();
    for i in 0..m.row_count() {
        if let Some(r) = m.row_data(i) {
            if r.checked {
                set.insert(r.path.to_string());
            }
        }
    }
    set
}

/// "N of M checked" + counted Push label from the visible checklist.
/// Push uploads local files to the repo; getting repo state onto this device
/// happens only in Download mode (Apply there, always with backup first).
fn show_app_count(ui: &AppWindow) {
    let m = ui.get_files();
    let mut chosen = 0usize;
    let mut push_n = 0usize;
    for i in 0..m.row_count() {
        if let Some(r) = m.row_data(i) {
            if r.checked {
                chosen += 1;
                if r.state == 1 || r.state == 2 {
                    push_n += 1;
                }
            }
        }
    }
    ui.set_has_selection(chosen > 0);
    ui.set_app_detail(format!("{chosen} of {} checked", m.row_count()).into());
    ui.set_push_label(if push_n == 0 {
        "Push".into()
    } else {
        format!("Push · {push_n}").into()
    });
}

/// Download-picker selection state for the disabled Apply button.
/// UI-local (reads models only, no filesystem) — safe on the UI thread.
fn show_repo_selection(ui: &AppWindow) {
    let m = ui.get_repo_files();
    let mut chosen = false;
    for i in 0..m.row_count() {
        if let Some(r) = m.row_data(i) {
            if r.checked {
                chosen = true;
                break;
            }
        }
    }
    ui.set_has_repo_selection(chosen);
}

/// Resolve a picked backup display name back to its full path.
fn backup_full_path(cfg: &Path, name: &str) -> Option<PathBuf> {
    for a in ocs_core::apps::discover_apps(cfg) {
        for b in store::list_backups(cfg, &a) {
            if b.backup.file_name().and_then(|s| s.to_str()) == Some(name) {
                return Some(b.backup);
            }
        }
    }
    None
}

/// Per-app download preview for the confirm dialog (all Send).
struct PreviewRow {
    id: String,
    writes: usize,
    error: Option<String>,
}

/// Pure download execution (no UI): preview -> backup -> full apply per app.
/// Runs on a background thread; the caller refreshes via a follow-up scan.
fn execute_download(cfg: &Path, repo: &Path, ids: &[String]) -> String {
    let mut log = String::new();
    for id in ids {
        let app = ocs_core::apps::resolve_app(cfg, id);
        let repo_app = repo.join(&app.id);
        match store::preview(cfg, &repo_app, &app) {
            Ok(plan) => {
                let extras = store::list_local_rels(cfg, &app)
                    .map(|(v, _)| v)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|f| !plan.iter().any(|o| o.rel == f.rel))
                    .count();
                match store::apply_plan(cfg, &repo_app, &app, &plan) {
                    Ok(r) => log.push_str(&format!(
                        "- {id}: wrote {}, unchanged {}, {extras} local-only → backup\n",
                        r.written, r.unchanged
                    )),
                    Err(e) => log.push_str(&format!("- {id}: aborted: {e}\n")),
                }
            }
            Err(e) => log.push_str(&format!("- {id}: preview failed: {e}\n")),
        }
    }
    format!("download:\n{log}")
}

/// Confirmed download as a background job with follow-up scan.
fn start_download_job(ui: &AppWindow, shared: &Shared, ids: Vec<String>) {
    let (cfg, repo) = config_and_repo(ui);
    let sh2 = shared.clone();
    run_job(
        ui,
        shared,
        "Applying",
        move || execute_download(&cfg, &repo, &ids),
        move |ui, job| {
            let took = job.took();
            let Some(log) = job.value(ui, "Download") else {
                start_scan(ui, &sh2, false, false);
                return;
            };
            ui.set_log_text(format!("download{took}:\n{log}").into());
            ui.set_copy_label("Copy".into());
            refresh_github(ui, &sh2);
            start_scan(ui, &sh2, false, false);
        },
    );
}

/// Snapshot every selected app into the repo clone (first-snapshot flows).
/// Shared by Init and Set up — one implementation, no drift.
/// Returns (apps, files, summed skips); stops at the first failing app.
fn snapshot_many(
    cfg: &Path,
    repo: &Path,
    sel: &[String],
) -> Result<(usize, usize, store::SkipStats), (String, String)> {
    let mut done = 0usize;
    let mut total = 0usize;
    let mut skipped = store::SkipStats::default();
    for id in sel {
        let app = ocs_core::apps::resolve_app(cfg, id);
        let all: HashSet<PathBuf> = store::list_local_rels(cfg, &app)
            .map(|(v, _)| v)
            .unwrap_or_default()
            .into_iter()
            .map(|f| f.rel)
            .collect();
        match store::snapshot_selected(cfg, &repo.join(&app.id), &app, &all) {
            Ok(r) => {
                total += r.copied;
                skipped.add(&r.skipped);
                done += 1;
            }
            Err(e) => return Err((id.clone(), e.to_string())),
        }
    }
    Ok((done, total, skipped))
}

fn checked_repo_apps(ui: &AppWindow) -> Vec<String> {
    let m = ui.get_repo_files();
    let mut v = Vec::new();
    for i in 0..m.row_count() {
        if let Some(r) = m.row_data(i) {
            if r.checked {
                v.push(r.path.to_string());
            }
        }
    }
    v
}

/// Pending destructive op behind the confirm dialog (preview -> confirm ->
/// backup -> copy). Only ids are stored; preview + checks are re-read on
/// accept so the dialog can never apply stale state.
#[derive(Debug, Clone)]
enum PendingOp {
    Download { ids: Vec<String> },
}

fn refresh_repo(ui: &AppWindow) {
    if ui.get_repo_dir().trim().is_empty() {
        ui.set_repo_status("no repository path set".into());
        ui.set_show_ssh_fix(false);
        return;
    }
    let repo = expand(&ui.get_repo_dir());
    match git::repo_status(&repo) {
        Ok(st) if !st.is_repo => {
            ui.set_repo_status("no repository — open Details → Set up".into());
            ui.set_show_ssh_fix(false);
        }
        Ok(st) => {
            let health = if st.clean { "clean" } else { "dirty" };
            let remote = if st.remote_url.is_empty() {
                "no remote".to_string()
            } else {
                st.remote_url.clone()
            };
            let flow = if st.ahead == 0 && st.behind == 0 {
                "up to date".to_string()
            } else {
                format!("↑{} ↓{} (vs last fetch)", st.ahead, st.behind)
            };
            ui.set_repo_status(format!("{} · {health} · {flow} · {remote}", st.branch).into());
            // HTTPS remotes can't use the SSH key and always fail auth:
            // offer the one-click switch in Details.
            let lower = st.remote_url.to_lowercase();
            ui.set_show_ssh_fix(lower.starts_with("https://") || lower.starts_with("http://"));
        }
        Err(e) => {
            ui.set_repo_status(format!("repo error: {e}").into());
            ui.set_show_ssh_fix(false);
        }
    }
}

/// Minimal GitHub header: one headline + status line, everything else
/// automatic. Never touches the welcome screen: it shows on every startup
/// (unless "Don't show again" was ticked) and closes only via explicit
/// user action (see [`close_welcome`]).
/// Takes a probed [`git::GhAuth`] so callers that already probed (startup,
/// auto-tick) don't spawn `gh` twice.
fn refresh_github_with(ui: &AppWindow, auth: &git::GhAuth) {
    ui.set_github_connected(auth.logged_in);
    ui.set_github_user(auth.user.clone().into());
    let repo = expand(&ui.get_repo_dir());
    let state = git::setup_state_for(auth.logged_in, &repo);
    if !auth.logged_in {
        ui.set_github_line("Not signed in to GitHub".into());
        ui.set_repo_status("Sign in in the browser — then everything runs automatically".into());
        ui.set_sync_label("Sync".into());
        return;
    }
    let repo_name = git::repo_dir_name(&repo);
    if state == git::SetupState::NeedRepo {
        ui.set_github_line(format!("{user} · no repository yet", user = auth.user).into());
        ui.set_repo_status(format!("{repo_name} will be created during setup").into());
        ui.set_sync_label("Sync".into());
        return;
    }
    // Ready: headline = who + where + flow, details stay in repo-status.
    refresh_repo(ui);
    let flow: String = ui.get_repo_status().to_string();
    let who = if auth.user.is_empty() {
        "GitHub".to_string()
    } else {
        auth.user.clone()
    };
    // Keep the headline short: user · repo · branch-ish flow prefix.
    let short = flow.split(" · ").next().unwrap_or(flow.as_str());
    ui.set_github_line(format!("{who} · {repo_name} · {short}").into());
    ui.set_sync_label("Sync".into());
}

/// Header refresh from the CACHED auth (instant, never probes).
/// Freshness comes from [`poll_auth`]; staleness ≤ one tick is fine here.
fn refresh_github(ui: &AppWindow, shared: &Shared) {
    refresh_github_with(ui, &lock(&shared.auth).clone());
}

/// Background `gh` probe; on arrival updates cache + header, and on a login
/// *change* scans + logs. Never blocks the UI thread. `close_on_login` is
/// only for the device flow (welcome closes once the login is verified,
/// never on a mere background tick).
fn poll_auth(weak: slint::Weak<AppWindow>, shared: Shared, close_on_login: bool) {
    if shared.auth_busy.swap(true, Ordering::SeqCst) {
        return; // one probe at a time; the running one delivers
    }
    std::thread::spawn(move || {
        let auth = git::gh_auth_status();
        *lock(&shared.auth) = auth.clone();
        let _ = slint::invoke_from_event_loop(move || {
            shared.auth_busy.store(false, Ordering::SeqCst);
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let was = ui.get_github_connected();
            refresh_github_with(&ui, &auth);
            if auth.logged_in && !was {
                if close_on_login {
                    close_welcome(&ui);
                }
                // No scan log: the sign-in message below wins.
                start_scan(&ui, &shared, false, false);
                ui.set_log_text(
                    format!(
                        "signed in as {} — welcome! Set up the repository under Details if needed.",
                        auth.user
                    )
                    .into(),
                );
            }
        });
    });
}

/// Close the welcome screen via explicit user action. Ticks "Don't show
/// again" persist to disk, so the screen stays away on future startups
/// (deleting the prefs file brings it back).
fn close_welcome(ui: &AppWindow) {
    if ui.get_welcome_never() {
        let _ = ocs_core::prefs::save_never_show(true);
    }
    ui.set_welcome_dismissed(true);
    ui.set_show_welcome(false);
}

/// Open Details on auth failures (the Use-SSH fix lives there).
/// Typed kinds, never substring-matching — compiler-checked.
fn reveal_on_auth(ui: &AppWindow, kind: git::ErrorKind) {
    if kind == git::ErrorKind::Auth {
        ui.set_show_repo_details(true);
    }
}

/// Run the device login on a background thread; the one-time code and the
/// completion are forwarded to the UI thread. Shared by the welcome screen
/// and the Details "Login via Web" button: the code shows in both places,
/// completion refreshes everything and re-probes SSH.
fn spawn_device_login(weak: slint::Weak<AppWindow>, shared: &Shared) {
    let weak_done = weak.clone();
    let sh = shared.clone();
    // Fresh run: an old cancel must not kill the new `gh` instantly.
    sh.login_cancel.store(false, Ordering::SeqCst);
    let cancel = sh.login_cancel.clone();
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || git::run_device_login(tx, &cancel));
        for ev in rx {
            match ev {
                git::LoginEvent::Code(c) => {
                    let w = weak.clone();
                    let code = c.code.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            ui.set_login_code(code.clone().into());
                            // Auto-copy so pasting into the browser is one
                            // keystroke; silent (the copy button covers failure).
                            let _ = ocs_core::clipboard::copy_text(&code);
                            if ui.get_show_welcome() {
                                ui.set_welcome_detail(
                                    "Enter this code in the browser, then approve.".into(),
                                );
                            } else {
                                ui.set_log_text(
                                    format!(
                                        "login code: {code} — enter it at github.com/login/device"
                                    )
                                    .into(),
                                );
                            }
                        }
                    });
                }
                git::LoginEvent::Cancelled => {
                    let w = weak_done.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            ui.set_login_busy(false);
                            ui.set_login_code("".into());
                            if ui.get_show_welcome() {
                                ui.set_welcome_detail("Login cancelled — nothing changed.".into());
                            }
                            ui.set_log_text("login cancelled — nothing changed".into());
                        }
                    });
                }
                git::LoginEvent::Done(res) => {
                    let w = weak_done.clone();
                    let sh2 = sh.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            ui.set_login_busy(false);
                            match res {
                                Ok(msg) => {
                                    ui.set_login_code("".into());
                                    ui.set_log_text(msg.into());
                                    // Verified refresh (cache + header + scan
                                    // on change) arrives via poll; SSH too.
                                    poll_auth(w.clone(), sh2.clone(), true);
                                    probe_ssh(w.clone(), &sh2);
                                }
                                Err(e) => {
                                    if ui.get_show_welcome() {
                                        ui.set_welcome_detail(format!("{e}").into());
                                    }
                                    ui.set_log_text(format!("login failed:\n{e}").into());
                                }
                            }
                        }
                    });
                }
            }
        }
    });
}

/// SSH readiness probe on a background thread (see [`git::ssh_status`]).
/// Run at startup, when Details opens, and after login. Superseded probes
/// stay silent via the generation guard (no last-writer-wins flicker).
fn probe_ssh(weak: slint::Weak<AppWindow>, shared: &Shared) {
    let my = shared.ssh_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let gen = shared.ssh_gen.clone();
    std::thread::spawn(move || {
        let st = git::ssh_status();
        let _ = slint::invoke_from_event_loop(move || {
            if gen.load(Ordering::SeqCst) != my {
                return;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_ssh_state(if st.ok { 1 } else { 2 });
                ui.set_ssh_detail(st.detail.into());
            }
        });
    });
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    apply_theme(&ui, &theme::load());
    ui.set_config_dir(
        scanner::resolve_config_dir()
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    ui.set_repo_dir(
        expand("~/config-data")
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    let theme_fp: Rc<RefCell<String>> = Rc::new(RefCell::new(theme::fingerprint_live()));
    // Shared job state (Send): background scans and mutating jobs read/write
    // through short locks — no Rc snapshots, no stale data, no UI freezes.
    // Models start empty; the startup scan below fills them (covered by the
    // fade-in, usually before it finishes).
    let shared = Shared::default();

    // Minimal startup: exactly one `gh` probe. The scan itself waits for
    // `startup()` (fired by the one-shot boot timer once the event loop
    // runs) so its completion handler can never be lost before `run()`.
    // Welcome shows on every start unless "Don't show again" was ticked
    // (persisted in prefs).
    let never = ocs_core::prefs::load_never_show();
    ui.set_welcome_never(never);
    ui.set_show_welcome(!never);
    // Exactly one `gh` probe, pre-loop (no UI to freeze yet); the result
    // seeds the auth cache every later refresh reads from.
    let auth = git::gh_auth_status();
    *lock(&shared.auth) = auth.clone();
    refresh_github_with(&ui, &auth);
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_startup(move || {
            let ui = ui_handle.unwrap();
            start_scan(&ui, &sh, false, true);
            if ui.get_show_welcome() && !ui.get_github_connected() {
                ui.set_welcome_detail(
                    "Never signed in — one click opens the browser with a code.".into(),
                );
            }
            ui.set_ready(true);
            // SSH readiness is probed in the background (Details shows it).
            probe_ssh(ui.as_weak(), &sh);
        });
    }

    // Live theme: re-apply silently when the Omarchy theme changes.
    // Dots/lists refresh via a background scan, never on the UI thread.
    {
        let ui_handle = ui.as_weak();
        let theme_fp = theme_fp.clone();
        let sh = shared.clone();
        ui.on_theme_tick(move || {
            let ui = ui_handle.unwrap();
            let fp = theme::fingerprint_live();
            if fp == *theme_fp.borrow() {
                return;
            }
            *theme_fp.borrow_mut() = fp;
            let t = theme::load();
            apply_theme(&ui, &t);
            start_scan(&ui, &sh, false, false);
            ui.set_log_text(
                format!("theme switched to {} — colors updated", t.display_name).into(),
            );
        });
    }

    // SCAN (read-only): checked apps only. Re-discovers (new configs appear).
    // Compute runs in the background; a scan always clears the filter first
    // so the summary covers every app.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_scan(move || {
            let ui = ui_handle.unwrap();
            if ui.get_show_confirm() {
                return;
            }
            ui.set_app_filter("".into());
            start_scan(&ui, &sh, false, true);
            refresh_github(&ui, &sh);
        });
    }

    // Sidebar app select.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_activate(move |app_id| {
            let ui = ui_handle.unwrap();
            remember(&ui, &sh.checks);
            let id = app_id.to_string();
            for_each_app_row(&ui, |m, i, mut r| {
                r.active = r.id.as_str() == id;
                m.set_row_data(i, r);
            });
            persist_sidebar(&ui, &sh.app_state);
            ui.set_active_app(id.clone().into());
            start_scan(&ui, &sh, false, false);
        });
    }

    // Sidebar checkbox: dots + summary refresh via background scan.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_app_toggled(move |app_id, v| {
            let ui = ui_handle.unwrap();
            for_each_app_row(&ui, |m, i, mut r| {
                if r.id.as_str() == app_id.as_str() {
                    r.checked = v;
                    m.set_row_data(i, r);
                }
            });
            persist_sidebar(&ui, &sh.app_state);
            start_scan(&ui, &sh, false, false);
        });
    }

    // Sidebar live filter (state-preserving: hidden apps keep checks).
    // Keystrokes obsolete each other via the scan generation guard.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_app_filter_changed(move |_needle| {
            let ui = ui_handle.unwrap();
            persist_sidebar(&ui, &sh.app_state);
            start_scan(&ui, &sh, false, false);
        });
    }

    // Upload/download mode toggle.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_mode_toggle(move || {
            let ui = ui_handle.unwrap();
            remember(&ui, &sh.checks);
            let now = !ui.get_download_mode();
            ui.set_download_mode(now);
            if now {
                ui.set_log_text("download mode — tick repo apps, then Apply all".into());
            } else {
                ui.set_log_text("upload mode — tick local files, then Push".into());
            }
            start_scan(&ui, &sh, false, false);
        });
    }

    // File checklist.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_file_toggled(move |idx, v| {
            let ui = ui_handle.unwrap();
            let m = ui.get_files();
            if let Some(mut row) = m.row_data(idx as usize) {
                row.checked = v;
                m.set_row_data(idx as usize, row);
            }
            remember(&ui, &sh.checks);
            show_app_count(&ui);
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_repo_file_toggled(move |idx, v| {
            let ui = ui_handle.unwrap();
            let m = ui.get_repo_files();
            if let Some(mut row) = m.row_data(idx as usize) {
                row.checked = v;
                m.set_row_data(idx as usize, row);
            }
            show_repo_selection(&ui);
        });
    }
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_select_all(move || {
            let ui = ui_handle.unwrap();
            set_all_checks(&ui, true);
            remember(&ui, &sh.checks);
            show_app_count(&ui);
        });
    }
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_select_none(move || {
            let ui = ui_handle.unwrap();
            set_all_checks(&ui, false);
            remember(&ui, &sh.checks);
            show_app_count(&ui);
        });
    }

    // PUSH (checked files: config -> repo -> commit -> push).
    // Snapshot + network run in the background; the UI stays alive.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_push(move |app_id| {
            let ui = ui_handle.unwrap();
            remember(&ui, &sh.checks);
            let id = app_id.to_string();
            let (cfg, repo) = config_and_repo(&ui);
            // No local clone yet (e.g. right after a fresh start): pushing
            // nowhere is impossible — point at Set up instead of running a
            // snapshot job that can only fail with a technical error.
            if repo.as_os_str().is_empty() || !repo.join(".git").exists() {
                ui.set_log_text("No repository yet — open Details → Set up first.".into());
                ui.set_show_repo_details(true);
                return;
            }
            let app = ocs_core::apps::resolve_app(&cfg, &id);
            let checked = read_checks(&ui);
            if checked.is_empty() {
                ui.set_log_text("nothing checked — tick files first".into());
                return;
            }
            let sh2 = sh.clone();
            run_job(
                &ui,
                &sh,
                "Pushing",
                move || {
                    let r =
                        match store::snapshot_selected(&cfg, &repo.join(&app.id), &app, &checked) {
                            Ok(r) => r,
                            Err(e) => {
                                return Err(git::GitError::other(format!("snapshot failed:\n{e}")))
                            }
                        };
                    let done_msg = |g: &str| {
                        format!(
                            "push {id}: {} files, {g}{}",
                            r.copied,
                            skip_suffix(&r.skipped)
                        )
                    };
                    match git::push_app(&repo, &app.id, &format!("sync {id}")) {
                        Ok(g) => Ok(done_msg(&g)),
                        // Remote repo gone (or never created): create it (attaches
                        // origin), then push the already-committed snapshot once.
                        // When creation fails because the repo already exists
                        // under our account, re-attach it directly instead.
                        Err(e) if e.kind() == git::ErrorKind::MissingRemote => {
                            let name = git::repo_dir_name(&repo);
                            match git::gh_create_remote(&repo, &name)
                                .and_then(|_| git::push_upstream(&repo))
                            {
                                Ok(_) => Ok(format!("{} + remote repo created", done_msg("pushed"))),
                                Err(e2) => match git::try_attach_existing(&repo) {
                                    Some(msg) => Ok(format!("{} + {msg}", done_msg("pushed"))),
                                    None => Err(git::GitError::with_kind(
                                        e2.kind(),
                                        format!(
                                            "snapshot ok ({} files), remote missing and create failed:\n{e2}",
                                            r.copied
                                        ),
                                    )),
                                },
                            }
                        }
                        Err(e) => Err(git::GitError::with_kind(
                            e.kind(),
                            format!("snapshot ok ({} files), push failed:\n{e}", r.copied),
                        )),
                    }
                },
                move |ui, job| {
                    let took = job.took();
                    let Some(res) = job.value(ui, "Push") else {
                        start_scan(ui, &sh2, false, false);
                        return;
                    };
                    match res {
                        Ok(msg) => ui.set_log_text(format!("{msg}{took}").into()),
                        Err(e) => {
                            reveal_on_auth(ui, e.kind());
                            ui.set_log_text(format!("{e}{took}").into());
                        }
                    }
                    ui.set_copy_label("Copy".into());
                    refresh_github(ui, &sh2);
                    start_scan(ui, &sh2, false, false);
                },
            );
        });
    }

    // DOWNLOAD (checked repo apps: preview -> CONFIRM -> backup -> full apply).
    // This is the only place that writes repo state onto this device.
    // Preview counting runs in the background; execution too.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_download(move || {
            let ui = ui_handle.unwrap();
            let (cfg, repo) = config_and_repo(&ui);
            let ids = checked_repo_apps(&ui);
            if ids.is_empty() {
                ui.set_log_text("nothing checked — tick repo apps first".into());
                return;
            }
            let sh2 = sh.clone();
            run_job(
                &ui,
                &sh,
                "Checking",
                move || {
                    let mut rows = Vec::new();
                    for id in &ids {
                        let app = ocs_core::apps::resolve_app(&cfg, id);
                        match store::preview(&cfg, &repo.join(&app.id), &app) {
                            Ok(plan) => {
                                let writes = plan
                                    .iter()
                                    .filter(|o| o.kind != store::OpKind::Unchanged)
                                    .count();
                                rows.push(PreviewRow {
                                    id: id.clone(),
                                    writes,
                                    error: None,
                                });
                            }
                            Err(e) => rows.push(PreviewRow {
                                id: id.clone(),
                                writes: 0,
                                error: Some(e.to_string()),
                            }),
                        }
                    }
                    (ids, rows)
                },
                move |ui, job| {
                    let Some((ids, rows)) = job.value(ui, "Check") else {
                        return;
                    };
                    let writes: usize = rows.iter().map(|r| r.writes).sum();
                    let mut lines = Vec::new();
                    for r in &rows {
                        match &r.error {
                            Some(err) => lines.push(format!("- {}: preview failed: {err}", r.id)),
                            None => lines.push(format!("- {}: {} to write", r.id, r.writes)),
                        }
                    }
                    if writes == 0 {
                        if rows.iter().any(|r| r.error.is_some()) {
                            ui.set_log_text(
                                format!("download preview failed:\n{}", lines.join("\n")).into(),
                            );
                            return;
                        }
                        start_download_job(ui, &sh2, ids);
                        return;
                    }
                    *lock(&sh2.pending) = Some(PendingOp::Download { ids: ids.clone() });
                    ui.set_confirm_title(
                        format!("Apply {} from the repository?", ids.len()).into(),
                    );
                    ui.set_confirm_detail(lines.join("\n").into());
                    ui.set_confirm_ok_label("Backup + Apply".into());
                    ui.set_show_confirm(true);
                },
            );
        });
    }

    // CONFIRM dialog actions.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_confirm_accept(move || {
            let ui = ui_handle.unwrap();
            let op = lock(&sh.pending).take();
            ui.set_show_confirm(false);
            match op {
                Some(PendingOp::Download { ids }) => start_download_job(&ui, &sh, ids),
                None => {}
            }
        });
    }
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_confirm_cancel(move || {
            let ui = ui_handle.unwrap();
            *lock(&sh.pending) = None;
            ui.set_show_confirm(false);
            ui.set_log_text("cancelled — nothing changed".into());
        });
    }

    // RESTORE (copy a .bak.* back, file by file, never deleting).
    // Lookup + copy run in the background; the UI stays alive.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_restore(move |bak_path| {
            let ui = ui_handle.unwrap();
            let (cfg, _) = config_and_repo(&ui);
            let bak = expand(&bak_path);
            let sh2 = sh.clone();
            run_job(
                &ui,
                &sh,
                "Restoring",
                move || {
                    let mut found = None;
                    for a in ocs_core::apps::discover_apps(&cfg) {
                        for b in store::list_backups(&cfg, &a) {
                            if b.backup == bak {
                                found = Some(b);
                                break;
                            }
                        }
                    }
                    let Some(info) = found else {
                        return Err(git::GitError::other(
                            "backup not recognized — pick one from the Restore list below",
                        ));
                    };
                    match store::restore_backup(&info.backup, &info.original) {
                        Ok(r) => Ok(format!(
                            "restore: {} files back, leftovers (kept): {:?}",
                            r.restored, r.leftovers
                        )),
                        Err(e) => Err(git::GitError::other(format!("restore failed:\n{e}"))),
                    }
                },
                move |ui, job| {
                    let took = job.took();
                    let Some(res) = job.value(ui, "Restore") else {
                        start_scan(ui, &sh2, false, false);
                        return;
                    };
                    match res {
                        Ok(msg) => ui.set_log_text(format!("{msg}{took}").into()),
                        Err(e) => ui.set_log_text(format!("{e}{took}").into()),
                    }
                    ui.set_copy_label("Copy".into());
                    refresh_github(ui, &sh2);
                    start_scan(ui, &sh2, false, false);
                },
            );
        });
    }

    // Repository: fetch / pull / push / clone / init (autonomous).
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_repo_fetch(move || {
            let ui = ui_handle.unwrap();
            let repo = expand(&ui.get_repo_dir());
            let sh2 = sh.clone();
            run_job(
                &ui,
                &sh,
                "Fetching",
                move || git::fetch(&repo),
                move |ui, job| {
                    let took = job.took();
                    let Some(res) = job.value(ui, "Fetch") else {
                        start_scan(ui, &sh2, false, false);
                        return;
                    };
                    match res {
                        Ok(_) => ui.set_log_text(
                            format!("checked for updates — status is vs remote now{took}").into(),
                        ),
                        Err(e) => {
                            reveal_on_auth(ui, e.kind());
                            ui.set_log_text(format!("update check failed:\n{e}{took}").into());
                        }
                    }
                    ui.set_copy_label("Copy".into());
                    refresh_github(ui, &sh2);
                    start_scan(ui, &sh2, false, false);
                },
            );
        });
    }
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_repo_pull(move || {
            let ui = ui_handle.unwrap();
            let repo = expand(&ui.get_repo_dir());
            if repo.as_os_str().is_empty() || !repo.join(".git").exists() {
                ui.set_log_text("No repository yet — open Details → Set up first.".into());
                ui.set_show_repo_details(true);
                return;
            }
            let sh2 = sh.clone();
            run_job(
                &ui,
                &sh,
                "Pulling",
                move || git::pull_ff_only(&repo),
                move |ui, job| {
                    let took = job.took();
                    let Some(res) = job.value(ui, "Pull") else {
                        start_scan(ui, &sh2, false, false);
                        return;
                    };
                    match res {
                        Ok(out) => ui.set_log_text(format!("pull ok:\n{out}{took}").into()),
                        Err(e) => {
                            reveal_on_auth(ui, e.kind());
                            ui.set_log_text(format!("pull failed:\n{e}{took}").into());
                        }
                    }
                    ui.set_copy_label("Copy".into());
                    refresh_github(ui, &sh2);
                    start_scan(ui, &sh2, false, false);
                },
            );
        });
    }
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_repo_push(move || {
            let ui = ui_handle.unwrap();
            let repo = expand(&ui.get_repo_dir());
            if repo.as_os_str().is_empty() || !repo.join(".git").exists() {
                ui.set_log_text("No repository yet — open Details → Set up first.".into());
                ui.set_show_repo_details(true);
                return;
            }
            let sh2 = sh.clone();
            run_job(
                &ui,
                &sh,
                "Pushing",
                move || git::push(&repo),
                move |ui, job| {
                    let took = job.took();
                    let Some(res) = job.value(ui, "Push") else {
                        start_scan(ui, &sh2, false, false);
                        return;
                    };
                    match res {
                        Ok(msg) => ui.set_log_text(format!("{msg}{took}").into()),
                        Err(e) => {
                            reveal_on_auth(ui, e.kind());
                            ui.set_log_text(format!("push failed:\n{e}{took}").into());
                        }
                    }
                    ui.set_copy_label("Copy".into());
                    refresh_github(ui, &sh2);
                    start_scan(ui, &sh2, false, false);
                },
            );
        });
    }
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_repo_clone(move || {
            let ui = ui_handle.unwrap();
            let url = ui.get_remote_url().trim().to_string();
            let dest = expand(&ui.get_repo_dir());
            if url.is_empty() {
                ui.set_log_text("enter a GitHub URL first".into());
                return;
            }
            let sh2 = sh.clone();
            run_job(
                &ui,
                &sh,
                "Cloning",
                move || {
                    git::clone_repo(&url, &dest).map(|_| ()).map_err(|e| {
                        git::GitError::with_kind(e.kind(), format!("clone failed:\n{e}"))
                    })
                },
                move |ui, job| {
                    let took = job.took();
                    let Some(res) = job.value(ui, "Clone") else {
                        start_scan(ui, &sh2, false, false);
                        return;
                    };
                    match res {
                        Ok(()) => {
                            ui.set_log_text(
                                format!("cloned — switched to download mode{took}").into(),
                            );
                            ui.set_download_mode(true);
                        }
                        Err(e) => {
                            reveal_on_auth(ui, e.kind());
                            ui.set_log_text(format!("clone failed:\n{e}{took}").into());
                        }
                    }
                    ui.set_copy_label("Copy".into());
                    refresh_github(ui, &sh2);
                    start_scan(ui, &sh2, false, false);
                },
            );
        });
    }
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_repo_init(move || {
            let ui = ui_handle.unwrap();
            let (cfg, repo) = config_and_repo(&ui);
            let url = ui.get_remote_url().trim().to_string();
            if ui.get_repo_dir().trim().is_empty() {
                ui.set_log_text("local repo path is empty — restart the app".into());
                return;
            }
            // Autonomous first snapshot: everything of the checked apps.
            let sel = selected_ids(&ui);
            let sh2 = sh.clone();
            run_job(
                &ui,
                &sh,
                "Setting up",
                move || {
                    // Auth failures anywhere below open Details on arrival.
                    let mut auth_failed = false;
                    let mut fail = |e: &git::GitError| {
                        if e.kind() == git::ErrorKind::Auth {
                            auth_failed = true;
                        }
                    };
                    if let Err(e) = git::init_repo(&repo) {
                        return (format!("init failed:\n{e}"), auth_failed);
                    }
                    let (done, total, skipped) = match snapshot_many(&cfg, &repo, &sel) {
                        Ok(t) => t,
                        Err((id, e)) => {
                            return (format!("snapshot {id} failed:\n{e}"), auth_failed);
                        }
                    };
                    let mut log = format!(
                        "initialized — {done} apps, {total} files snapshotted{}\n",
                        skip_suffix(&skipped)
                    );
                    match git::commit_all(&repo, "initial sync") {
                        Ok(m) => log.push_str(&format!("commit: {m}\n")),
                        Err(e) => log.push_str(&format!("commit failed: {e}\n")),
                    }
                    if url.is_empty() {
                        // Fully autonomous: create the GitHub repo via gh, then publish.
                        let name = git::repo_dir_name(&repo);
                        match git::gh_create_repo(&repo, &name)
                            .and_then(|_| git::push_upstream(&repo))
                        {
                            Ok(_) => log.push_str("GitHub repo created + published"),
                            Err(e) => {
                                fail(&e);
                                match git::try_attach_existing(&repo) {
                                    Some(msg) => log.push_str(&msg),
                                    None => log.push_str(&format!(
                                        "local repo ready, GitHub create failed:\n{e}\nTip: paste a URL above (or create the empty repo on github.com), then Init again"
                                    )),
                                }
                            }
                        }
                    } else {
                        match git::set_remote(&repo, &url)
                            .and_then(|_| git::push_upstream(&repo))
                        {
                            Ok(_) => log.push_str("remote attached + published to GitHub"),
                            Err(e) => {
                                fail(&e);
                                log.push_str(&format!(
                                    "local repo ready, publish failed (create the empty GitHub repo first?):\n{e}"
                                ));
                            }
                        }
                    }
                    (log, auth_failed)
                },
                move |ui, job| {
                    let took = job.took();
                    let Some((log, auth_failed)) = job.value(ui, "Setup") else {
                        start_scan(ui, &sh2, false, false);
                        return;
                    };
                    ui.set_log_text(format!("{log}{took}").into());
                    if auth_failed {
                        ui.set_show_repo_details(true);
                    }
                    ui.set_copy_label("Copy".into());
                    refresh_github(ui, &sh2);
                    start_scan(ui, &sh2, false, false);
                },
            );
        });
    }

    // WELCOME + BROWSER LOGIN: one button, code shown in-app, browser opens.
    // `gh auth login --web` runs on a background thread; its one-time code
    // is forwarded to the UI as soon as it prints, completion refreshes all.
    // Only `Send` data crosses threads (owned snapshots, like the Sync flow).
    // When already signed in, the same button just continues into the app.
    // The Details "Login via Web" button reuses the same flow.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_github_login(move || {
            let ui = ui_handle.unwrap();
            if ui.get_login_busy() {
                return;
            }
            if ui.get_github_connected() {
                close_welcome(&ui);
                ui.set_log_text("Welcome back — pick apps on the left, then Push or Sync.".into());
                return;
            }
            if !git::gh_available() {
                ui.set_welcome_detail(
                    "gh CLI missing — install it (`omarchy install gh`), then try again.".into(),
                );
                return;
            }
            ui.set_login_busy(true);
            ui.set_login_code("".into());
            ui.set_welcome_detail("Starting sign-in — browser opens in a moment…".into());
            spawn_device_login(ui.as_weak(), &sh);
        });
    }
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_github_relogin(move || {
            let ui = ui_handle.unwrap();
            if ui.get_login_busy() {
                return;
            }
            if !git::gh_available() {
                ui.set_log_text(
                    "gh CLI missing — install it (`omarchy install gh`), then try again.".into(),
                );
                return;
            }
            ui.set_login_busy(true);
            ui.set_login_code("".into());
            ui.set_log_text("Starting sign-in — browser opens in a moment…".into());
            // Open the device page right away (don't wait for `gh` to get
            // around to it); the code auto-copies below when it prints.
            git::open_browser(git::DEVICE_URL);
            spawn_device_login(ui.as_weak(), &sh);
        });
    }
    {
        let sh = shared.clone();
        ui.on_cancel_login(move || {
            // The waiter kills `gh` and answers with Cancelled, which clears
            // busy state + code; the flag alone never touches the UI.
            sh.login_cancel.store(true, Ordering::SeqCst);
        });
    }
    {
        ui.on_open_device_page(move || {
            git::open_browser(git::DEVICE_URL);
        });
    }
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_github_recheck(move || {
            let ui = ui_handle.unwrap();
            // Instant answer from cache; the background poll verifies and
            // self-heals (scan + log) if you literally just logged in.
            refresh_github(&ui, &sh);
            poll_auth(ui.as_weak(), sh.clone(), false);
            if ui.get_github_connected() {
                close_welcome(&ui);
                // No scan log: the sign-in message below wins.
                start_scan(&ui, &sh, false, false);
                let who = ui.get_github_user().to_string();
                ui.set_log_text(
                    format!("signed in as {who} — set up the repository under Details if needed.")
                        .into(),
                );
            } else {
                ui.set_welcome_detail(
                    "Not signed in yet — finish the browser window, then tap again.".into(),
                );
            }
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_welcome_dismiss(move || {
            let ui = ui_handle.unwrap();
            close_welcome(&ui);
            ui.set_log_text(
                "Offline mode — Scan/Push work locally, Sync needs a login later.".into(),
            );
        });
    }

    // SYNC: one button instead of check-updates/pull/push. Runs through the
    // shared job helper (busy + panic-proof + timing) like everything else.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_github_sync(move || {
            let ui = ui_handle.unwrap();
            let repo = expand(&ui.get_repo_dir());
            if git::setup_state_for(ui.get_github_connected(), &repo) != git::SetupState::Ready {
                ui.set_log_text("No repository yet — open Details → Set up first.".into());
                ui.set_show_repo_details(true);
                return;
            }
            let sh2 = sh.clone();
            run_job(
                &ui,
                &sh,
                "Syncing",
                move || git::sync_repo(&repo),
                move |ui, job| {
                    let took = job.took();
                    let Some(res) = job.value(ui, "Sync") else {
                        start_scan(ui, &sh2, false, false);
                        return;
                    };
                    match res {
                        Ok(s) => {
                            ui.set_log_text(format!("sync: {s}{took}").into());
                        }
                        Err(e) => {
                            // Auth failures are actionable in Details (Use SSH
                            // button): open it instead of leaving the user
                            // staring at the log.
                            reveal_on_auth(ui, e.kind());
                            ui.set_log_text(format!("sync failed:\n{e}{took}").into());
                        }
                    }
                    ui.set_copy_label("Copy".into());
                    refresh_github(ui, &sh2);
                    start_scan(ui, &sh2, false, false);
                },
            );
        });
    }

    // SET UP (smart): clone when URL + target missing, else init + publish.
    // Repos that already exist are just synced. Everything blocking runs in
    // the background; the UI stays alive.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_repo_setup(move || {
            let ui = ui_handle.unwrap();
            let url = ui.get_remote_url().trim().to_string();
            let dest = expand(&ui.get_repo_dir());
            if dest.join(".git").exists() {
                let sh2 = sh.clone();
                run_job(
                    &ui,
                    &sh,
                    "Syncing",
                    move || {
                        git::sync_repo(&dest)
                            .map(|s| format!("already set up — sync: {s}"))
                            .map_err(|e| {
                                git::GitError::with_kind(
                                    e.kind(),
                                    format!("sync failed:\n{e}"),
                                )
                            })
                    },
                    move |ui, job| {
                        let took = job.took();
                        let Some(res) = job.value(ui, "Sync") else {
                            start_scan(ui, &sh2, false, false);
                            return;
                        };
                        match res {
                            Ok(msg) => ui.set_log_text(format!("{msg}{took}").into()),
                            Err(e) => {
                                reveal_on_auth(ui, e.kind());
                                ui.set_log_text(format!("{e}{took}").into());
                            }
                        }
                        ui.set_copy_label("Copy".into());
                        refresh_github(ui, &sh2);
                        start_scan(ui, &sh2, false, false);
                    },
                );
                return;
            }
            if !url.is_empty() && !dest.exists() {
                let sh2 = sh.clone();
                run_job(
                    &ui,
                    &sh,
                    "Cloning",
                    move || {
                        git::clone_repo(&url, &dest)
                            .map(|_| ())
                            .map_err(|e| {
                                git::GitError::with_kind(e.kind(), format!("clone failed:\n{e}"))
                            })
                    },
                    move |ui, job| {
                        let took = job.took();
                        let Some(res) = job.value(ui, "Clone") else {
                            start_scan(ui, &sh2, false, false);
                            return;
                        };
                        match res {
                            Ok(()) => {
                                ui.set_log_text(
                                    format!("cloned — switched to download mode{took}").into(),
                                );
                                ui.set_download_mode(true);
                            }
                            Err(e) => {
                                reveal_on_auth(ui, e.kind());
                                ui.set_log_text(format!("clone failed:\n{e}{took}").into());
                            }
                        }
                        ui.set_copy_label("Copy".into());
                        refresh_github(ui, &sh2);
                        start_scan(ui, &sh2, false, false);
                    },
                );
                return;
            }
            let (cfg, repo) = config_and_repo(&ui);
            if ui.get_repo_dir().trim().is_empty() {
                ui.set_log_text("Local path is empty — restart the app, or enter a GitHub URL to clone.".into());
                return;
            }
            if !lock(&sh.auth).logged_in && url.is_empty() {
                // Cache read (instant); a freshness probe runs alongside in
                // case you literally just logged in — retry in a second then.
                poll_auth(ui.as_weak(), sh.clone(), false);
                ui.set_log_text(
                    "Local-only without login — sign in in the browser first, then Set up."
                        .into(),
                );
                return;
            }
            let sel = selected_ids(&ui);
            let sh2 = sh.clone();
            run_job(
                &ui,
                &sh,
                "Setting up",
                move || {
                    let mut auth_failed = false;
                    if let Err(e) = git::init_repo(&repo) {
                        return (format!("init failed:\n{e}"), auth_failed);
                    }
                    let (done, total, skipped) = match snapshot_many(&cfg, &repo, &sel) {
                        Ok(t) => t,
                        Err((id, e)) => return (format!("snapshot {id} failed:\n{e}"), auth_failed),
                    };
                    let mut log =
                        format!("set up — {done} apps, {total} files{}\n", skip_suffix(&skipped));
            match git::commit_all(&repo, "initial sync") {
                Ok(m) => log.push_str(&format!("commit: {m}\n")),
                Err(e) => log.push_str(&format!("commit failed: {e}\n")),
            }
            if url.is_empty() {
                let name = git::repo_dir_name(&repo);
                match git::gh_create_repo(&repo, &name)
                    .and_then(|_| git::push_upstream(&repo))
                {
                    Ok(_) => log.push_str("GitHub repo created + published"),
                    Err(e) => {
                        if e.kind() == git::ErrorKind::Auth {
                            auth_failed = true;
                        }
                        match git::try_attach_existing(&repo) {
                            Some(msg) => log.push_str(&msg),
                            None => log.push_str(&format!(
                                "local repo ready, GitHub create failed:\n{e}\nTip: create the empty repo on github.com, paste its URL in Details, Set up again"
                            )),
                        }
                    }
                }
            } else {
                match git::set_remote(&repo, &url).and_then(|_| git::push_upstream(&repo)) {
                    Ok(_) => log.push_str("remote attached + published to GitHub"),
                    Err(e) => {
                        if e.kind() == git::ErrorKind::Auth {
                            auth_failed = true;
                        }
                        log.push_str(&format!(
                            "local repo ready, publish failed (create the empty GitHub repo first?):\n{e}"
                        ));
                    }
                }
            }
            (log, auth_failed)
                },
                move |ui, job| {
                    let took = job.took();
                    let Some((log, auth_failed)) = job.value(ui, "Setup") else {
                        start_scan(ui, &sh2, false, false);
                        return;
                    };
                    ui.set_log_text(format!("{log}{took}").into());
                    if auth_failed {
                        ui.set_show_repo_details(true);
                    }
                    ui.set_copy_label("Copy".into());
                    refresh_github(ui, &sh2);
                    start_scan(ui, &sh2, false, false);
                },
            );
        });
    }

    // USE SSH: one-click switch from an HTTPS remote to SSH, so syncs use
    // the key and never hit a username prompt again.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_fix_ssh_remote(move || {
            let ui = ui_handle.unwrap();
            if ui.get_busy() || ui.get_show_confirm() {
                return;
            }
            let repo = expand(&ui.get_repo_dir());
            let url = match git::repo_status(&repo) {
                Ok(st) => st.remote_url,
                Err(e) => {
                    ui.set_log_text(format!("remote check failed:\n{e}").into());
                    return;
                }
            };
            let fixed = git::normalize_github_ssh(&url);
            if fixed == url.trim() {
                ui.set_log_text(
                    "remote is already SSH — the failure is the key or login, not the URL.".into(),
                );
                return;
            }
            match git::set_remote(&repo, &fixed) {
                Ok(_) => {
                    ui.set_log_text(
                        format!("remote switched to SSH:\n{fixed}\nTry Sync again.").into(),
                    );
                }
                Err(e) => ui.set_log_text(format!("remote switch failed:\n{e}").into()),
            }
            refresh_github(&ui, &sh);
        });
    }

    // DETAILS expand/collapse: opening refreshes repo state and re-probes
    // SSH so the panel always shows fresh detection results.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_toggle_details(move || {
            let ui = ui_handle.unwrap();
            let open = !ui.get_show_repo_details();
            ui.set_show_repo_details(open);
            if open {
                refresh_github(&ui, &sh);
                poll_auth(ui.as_weak(), sh.clone(), false);
                probe_ssh(ui.as_weak(), &sh);
            }
        });
    }

    // RESTORE picker (Download mode): expand + pick a backup by tapping it.
    {
        let ui_handle = ui.as_weak();
        ui.on_toggle_restore(move || {
            let ui = ui_handle.unwrap();
            ui.set_show_restore(!ui.get_show_restore());
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_backup_pick(move |name| {
            let ui = ui_handle.unwrap();
            let (cfg, _) = config_and_repo(&ui);
            let name = name.to_string();
            match backup_full_path(&cfg, &name) {
                Some(full) => {
                    ui.set_restore_path(full.to_string_lossy().into_owned().into());
                    ui.set_restore_picked(name.into());
                }
                None => ui.set_log_text(format!("backup gone: {name}").into()),
            }
        });
    }

    // LOG copy button: clipboard for pasting into AI chats and such.
    {
        let ui_handle = ui.as_weak();
        ui.on_copy_log(move || {
            let ui = ui_handle.unwrap();
            if ocs_core::clipboard::copy_text(ui.get_log_text().as_ref()) {
                ui.set_copy_label("Copied ✓".into());
            } else {
                ui.set_copy_label("Copy failed".into());
            }
        });
    }
    // Login-code copy button (icon next to the code, Details panel).
    // Silent on success (just paste); a line in the log only on failure.
    {
        let ui_handle = ui.as_weak();
        ui.on_copy_code(move || {
            let ui = ui_handle.unwrap();
            if !ocs_core::clipboard::copy_text(ui.get_login_code().as_ref()) {
                ui.set_log_text("copy failed — install wl-copy (or xclip/xsel)".into());
            }
        });
    }

    // AUTO in the background: login detection + quiet fetch every 30s.
    // Exactly one `gh` probe per tick (guarded); detection keeps running in
    // offline mode so a later login is noticed immediately.
    // Skipped while a job is in flight (busy) to avoid overlapping git.
    // The tick itself never blocks: probe + fetch + compare all run in jobs.
    {
        let ui_handle = ui.as_weak();
        let sh = shared.clone();
        ui.on_auto_tick(move || {
            let ui = ui_handle.unwrap();
            if ui.get_busy() {
                return;
            }
            // Login changes (background, then scan + log on arrival).
            poll_auth(ui.as_weak(), sh.clone(), false);
            // Periodic refresh from cached auth (≤30 s stale — status only).
            let auth = lock(&sh.auth).clone();
            if !auth.logged_in {
                return;
            }
            if git::setup_state_for(true, &expand(&ui.get_repo_dir())) != git::SetupState::Ready {
                return;
            }
            if ui.get_show_welcome() {
                return;
            }
            refresh_github_with(&ui, &auth);
            start_scan(&ui, &sh, true, false);
        });
    }

    ui.run()
}
