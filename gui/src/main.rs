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
    match id {
        "zed" => "Zed".to_string(),
        "hypr" => "Hyprland".to_string(),
        "omarchy-shell" => "Omarchy shell".to_string(),
        _ => ocs_core::apps::resolve_app(Path::new(""), id).label,
    }
}

/// (favorites, rest) sidebar models. `keep` preserves checks + active state
/// across rescans (discovery may find new entries at any time).
fn sidebar_models(
    cfg: &Path,
    keep: &HashMap<String, (bool, bool)>,
) -> (Vec<AppEntry>, Vec<AppEntry>) {
    let mut favs = Vec::new();
    let mut rest = Vec::new();
    for a in ocs_core::apps::discover_apps(cfg) {
        // Favorites on, everything else off: enable extra apps explicitly.
        let (checked, active) = keep
            .get(&a.id)
            .copied()
            .unwrap_or((a.is_favorite(), a.id == "zed"));
        let e = AppEntry {
            id: a.id.clone().into(),
            name: a.label.clone().into(),
            checked,
            active,
            sub: "".into(),
            state: 0,
        };
        if a.is_favorite() {
            favs.push(e);
        } else {
            rest.push(e);
        }
    }
    (favs, rest)
}

fn set_sidebar(ui: &AppWindow, favs: Vec<AppEntry>, rest: Vec<AppEntry>) {
    ui.set_fav_apps(ModelRc::new(VecModel::from(favs)));
    ui.set_other_apps(ModelRc::new(VecModel::from(rest)));
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
fn remember(ui: &AppWindow, checks: &Rc<RefCell<HashMap<String, HashSet<PathBuf>>>>) {
    checks
        .borrow_mut()
        .insert(ui.get_active_app().to_string(), read_checks(ui));
}

/// "N of M checked" + counted Push/Apply labels from the visible checklist.
fn show_app_count(ui: &AppWindow) {
    let m = ui.get_files();
    let mut chosen = 0usize;
    let mut push_n = 0usize;
    let mut apply_n = 0usize;
    for i in 0..m.row_count() {
        if let Some(r) = m.row_data(i) {
            if r.checked {
                chosen += 1;
                match r.state {
                    1 => {
                        push_n += 1;
                        apply_n += 1;
                    }
                    2 => push_n += 1,
                    3 => apply_n += 1,
                    _ => {}
                }
            }
        }
    }
    ui.set_app_detail(format!("{chosen} of {} checked", m.row_count()).into());
    ui.set_push_label(if push_n == 0 {
        "Push".into()
    } else {
        format!("Push · {push_n}").into()
    });
    ui.set_apply_label(if apply_n == 0 {
        "Apply".into()
    } else {
        format!("Apply · {apply_n}").into()
    });
}

/// Rebuild the file list for `app_id`, preserving remembered checks.
fn show_app(
    ui: &AppWindow,
    cfg: &Path,
    repo: &Path,
    app_id: &str,
    checks: &HashMap<String, HashSet<PathBuf>>,
) {
    let app = ocs_core::apps::resolve_app(cfg, app_id);
    ui.set_app_title(app_title(app_id).into());
    let cmp = store::compare(cfg, &repo.join(&app.id), &app, None).unwrap_or_default();
    let remembered = checks.get(app_id);
    let entries: Vec<FileEntry> = cmp
        .into_iter()
        .map(|c| {
            let checked = remembered.map(|s| s.contains(&c.rel)).unwrap_or(true);
            FileEntry {
                path: c.rel.to_string_lossy().into_owned().into(),
                checked,
                note: format!("{} · {}", human_bytes(c.bytes), state_word(c.state)).into(),
                state: state_idx(c.state),
            }
        })
        .collect();
    ui.set_files(ModelRc::new(VecModel::from(entries)));
    show_app_count(ui);
}

fn refresh_model(
    m: &ModelRc<AppEntry>,
    cfg: &Path,
    repo: &Path,
    sel: &[String],
    synced: &mut usize,
    changed: &mut usize,
) {
    for i in 0..m.row_count() {
        let mut row = match m.row_data(i) {
            Some(r) => r,
            None => continue,
        };
        let id = row.id.to_string();
        if !sel.iter().any(|s| s == &id) {
            row.state = 0;
            row.sub = "skipped".into();
        } else {
            let app = ocs_core::apps::resolve_app(cfg, &id);
            let installed = app.rel_paths.iter().any(|r| cfg.join(r).exists());
            if !installed {
                row.state = 0;
                row.sub = "not installed".into();
            } else {
                let cmp = store::compare(cfg, &repo.join(&app.id), &app, None).unwrap_or_default();
                if cmp.is_empty() {
                    row.state = 0;
                    row.sub = "no files".into();
                } else {
                    let mut c = 0usize;
                    for f in &cmp {
                        match f.state {
                            store::FileState::Synced => *synced += 1,
                            _ => {
                                c += 1;
                                *changed += 1;
                            }
                        }
                    }
                    if c == 0 {
                        row.state = 1;
                        row.sub = format!("{} files · synced", cmp.len()).into();
                    } else {
                        row.state = 2;
                        row.sub = format!("{} files · {c} changed", cmp.len()).into();
                    }
                }
            }
        }
        m.set_row_data(i, row);
    }
}

/// Sidebar dots + subs + header summary across the checked apps.
fn refresh_all(ui: &AppWindow) {
    let (cfg, repo) = config_and_repo(ui);
    let sel = selected_ids(ui);
    let mut synced = 0usize;
    let mut changed = 0usize;
    refresh_model(
        &ui.get_fav_apps(),
        &cfg,
        &repo,
        &sel,
        &mut synced,
        &mut changed,
    );
    refresh_model(
        &ui.get_other_apps(),
        &cfg,
        &repo,
        &sel,
        &mut synced,
        &mut changed,
    );
    ui.set_sync_summary(if sel.is_empty() {
        "no apps selected".into()
    } else {
        format!("{synced} synced · {changed} changed").into()
    });
}

/// Repo download picker: one row per app found in the clone.
fn show_repo_apps(ui: &AppWindow, cfg: &Path, repo: &Path) {
    let mut entries = Vec::new();
    for ra in store::list_repo_apps(repo) {
        let app = ocs_core::apps::resolve_app(cfg, &ra.id);
        let cmp = store::compare(cfg, &repo.join(&app.id), &app, None).unwrap_or_default();
        let synced = cmp
            .iter()
            .filter(|c| c.state == store::FileState::Synced)
            .count();
        let dirty = synced != cmp.len();
        entries.push(FileEntry {
            path: ra.id.clone().into(),
            checked: dirty,
            note: format!(
                "{} files · {} · {synced}/{} synced",
                ra.files,
                human_bytes(ra.bytes),
                cmp.len()
            )
            .into(),
            state: if dirty { 1 } else { 0 },
        });
    }
    ui.set_repo_files(ModelRc::new(VecModel::from(entries)));
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

fn refresh_repo(ui: &AppWindow) {
    if ui.get_repo_dir().trim().is_empty() {
        ui.set_repo_status("no repository configured".into());
        return;
    }
    let repo = expand(&ui.get_repo_dir());
    match git::repo_status(&repo) {
        Ok(st) if !st.is_repo => {
            ui.set_repo_status("not a repository — Clone or Init below".into())
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
        }
        Err(e) => ui.set_repo_status(format!("repo error: {e}").into()),
    }
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
    let (cfg0, _) = config_and_repo(&ui);
    let (favs, rest) = sidebar_models(&cfg0, &HashMap::new());
    set_sidebar(&ui, favs, rest);
    ui.set_log_text("Pick apps on the left, set a repository above, then Scan.".into());
    refresh_repo(&ui);

    let checks: Rc<RefCell<HashMap<String, HashSet<PathBuf>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let theme_fp: Rc<RefCell<String>> = Rc::new(RefCell::new(theme::fingerprint_live()));

    // Live theme: re-apply silently when the Omarchy theme changes.
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        let theme_fp = theme_fp.clone();
        ui.on_theme_tick(move || {
            let ui = ui_handle.unwrap();
            let fp = theme::fingerprint_live();
            if fp == *theme_fp.borrow() {
                return;
            }
            *theme_fp.borrow_mut() = fp;
            let t = theme::load();
            apply_theme(&ui, &t);
            refresh_all(&ui);
            let (cfg, repo) = config_and_repo(&ui);
            if ui.get_download_mode() {
                show_repo_apps(&ui, &cfg, &repo);
            } else {
                show_app(
                    &ui,
                    &cfg,
                    &repo,
                    ui.get_active_app().as_str(),
                    &checks.borrow(),
                );
            }
            ui.set_log_text(
                format!("theme switched to {} — colors updated", t.display_name).into(),
            );
        });
    }

    // SCAN (read-only): checked apps only. Re-discovers (new configs appear).
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_scan(move || {
            let ui = ui_handle.unwrap();
            let (cfg, repo) = config_and_repo(&ui);
            let keep = snapshot_sidebar(&ui);
            let (favs, rest) = sidebar_models(&cfg, &keep);
            set_sidebar(&ui, favs, rest);
            let sel = selected_ids(&ui);
            refresh_all(&ui);
            if ui.get_download_mode() {
                show_repo_apps(&ui, &cfg, &repo);
            } else {
                show_app(
                    &ui,
                    &cfg,
                    &repo,
                    ui.get_active_app().as_str(),
                    &checks.borrow(),
                );
            }
            let mut log = String::from("scan (read-only):\n");
            for id in &sel {
                let app = ocs_core::apps::resolve_app(&cfg, id);
                let cmp = store::compare(&cfg, &repo.join(&app.id), &app, None).unwrap_or_default();
                let synced = cmp
                    .iter()
                    .filter(|c| c.state == store::FileState::Synced)
                    .count();
                log.push_str(&format!("- {id}: {synced}/{} synced\n", cmp.len()));
            }
            let others = scanner::scan_other_entries(&cfg, &ocs_core::apps::builtin_apps());
            log.push_str(&format!("other ~/.config entries: {}\n", others.len()));
            ui.set_log_text(log.into());
            refresh_repo(&ui);
        });
    }

    // Sidebar app select.
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_activate(move |app_id| {
            let ui = ui_handle.unwrap();
            remember(&ui, &checks);
            let id = app_id.to_string();
            for_each_app_row(&ui, |m, i, mut r| {
                r.active = r.id.as_str() == id;
                m.set_row_data(i, r);
            });
            ui.set_active_app(id.clone().into());
            let (cfg, repo) = config_and_repo(&ui);
            show_app(&ui, &cfg, &repo, &id, &checks.borrow());
        });
    }

    // Sidebar checkbox.
    {
        let ui_handle = ui.as_weak();
        ui.on_app_toggled(move |app_id, v| {
            let ui = ui_handle.unwrap();
            for_each_app_row(&ui, |m, i, mut r| {
                if r.id.as_str() == app_id.as_str() {
                    r.checked = v;
                    m.set_row_data(i, r);
                }
            });
        });
    }

    // Upload/download mode toggle.
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_mode_toggle(move || {
            let ui = ui_handle.unwrap();
            remember(&ui, &checks);
            let now = !ui.get_download_mode();
            ui.set_download_mode(now);
            let (cfg, repo) = config_and_repo(&ui);
            if now {
                show_repo_apps(&ui, &cfg, &repo);
                ui.set_log_text("download mode — tick repo apps, then Übernehmen".into());
            } else {
                show_app(
                    &ui,
                    &cfg,
                    &repo,
                    ui.get_active_app().as_str(),
                    &checks.borrow(),
                );
                ui.set_log_text("upload mode — tick local files, then Push".into());
            }
        });
    }

    // File checklist.
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_file_toggled(move |idx, v| {
            let ui = ui_handle.unwrap();
            let m = ui.get_files();
            if let Some(mut row) = m.row_data(idx as usize) {
                row.checked = v;
                m.set_row_data(idx as usize, row);
            }
            remember(&ui, &checks);
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
        });
    }
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_select_all(move || {
            let ui = ui_handle.unwrap();
            set_all_checks(&ui, true);
            remember(&ui, &checks);
            show_app_count(&ui);
        });
    }
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_select_none(move || {
            let ui = ui_handle.unwrap();
            set_all_checks(&ui, false);
            remember(&ui, &checks);
            show_app_count(&ui);
        });
    }

    // PUSH (checked files: config -> repo -> commit -> push).
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_push(move |app_id| {
            let ui = ui_handle.unwrap();
            remember(&ui, &checks);
            let id = app_id.to_string();
            let (cfg, repo) = config_and_repo(&ui);
            let app = ocs_core::apps::resolve_app(&cfg, &id);
            let checked = read_checks(&ui);
            if checked.is_empty() {
                ui.set_log_text("nothing checked — tick files first".into());
                return;
            }
            match store::snapshot_selected(&cfg, &repo.join(&app.id), &app, &checked) {
                Ok(n) => match git::push_app(&repo, &app.id, &format!("sync {id}")) {
                    Ok(g) => {
                        ui.set_log_text(format!("push {id}: {n} files, {g}").into());
                    }
                    Err(e) => ui
                        .set_log_text(format!("snapshot ok ({n} files), push failed:\n{e}").into()),
                },
                Err(e) => ui.set_log_text(format!("snapshot failed:\n{e}").into()),
            }
            refresh_all(&ui);
            show_app(&ui, &cfg, &repo, &id, &checks.borrow());
            refresh_repo(&ui);
        });
    }

    // APPLY (checked files: preview -> backup -> copy + merge-back).
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_apply(move |app_id| {
            let ui = ui_handle.unwrap();
            remember(&ui, &checks);
            let id = app_id.to_string();
            let (cfg, repo) = config_and_repo(&ui);
            let app = ocs_core::apps::resolve_app(&cfg, &id);
            let checked = read_checks(&ui);
            if checked.is_empty() {
                ui.set_log_text("nothing checked — tick files first".into());
                return;
            }
            let repo_app = repo.join(&app.id);
            match store::preview(&cfg, &repo_app, &app) {
                Ok(plan) => {
                    let plan: Vec<_> =
                        plan.into_iter().filter(|o| checked.contains(&o.rel)).collect();
                    match store::apply_plan_selected(&cfg, &repo_app, &app, &plan, &checked) {
                        Ok(r) => {
                            ui.set_log_text(
                                format!(
                                    "apply {id}: wrote {}, unchanged {}, merged back {}, backup: {:?}",
                                    r.written, r.unchanged, r.merged_back, r.backed_up
                                )
                                .into(),
                            );
                        }
                        Err(e) => ui.set_log_text(format!("apply aborted:\n{e}").into()),
                    }
                }
                Err(e) => ui.set_log_text(format!("preview failed:\n{e}").into()),
            }
            refresh_all(&ui);
            show_app(&ui, &cfg, &repo, &id, &checks.borrow());
        });
    }

    // DOWNLOAD (checked repo apps: preview -> backup -> full apply).
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_download(move || {
            let ui = ui_handle.unwrap();
            let (cfg, repo) = config_and_repo(&ui);
            let ids = checked_repo_apps(&ui);
            if ids.is_empty() {
                ui.set_log_text("nothing checked — tick repo apps first".into());
                return;
            }
            let mut log = String::new();
            for id in &ids {
                let app = ocs_core::apps::resolve_app(&cfg, id);
                let repo_app = repo.join(&app.id);
                match store::preview(&cfg, &repo_app, &app) {
                    Ok(plan) => {
                        let extras = store::list_local_rels(&cfg, &app)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|f| !plan.iter().any(|o| o.rel == f.rel))
                            .count();
                        match store::apply_plan(&cfg, &repo_app, &app, &plan) {
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
            ui.set_log_text(format!("download:\n{log}").into());
            refresh_all(&ui);
            show_repo_apps(&ui, &cfg, &repo);
            let _ = &checks;
        });
    }

    // RESTORE (copy a .bak.* back, file by file, never deleting).
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_restore(move |bak_path| {
            let ui = ui_handle.unwrap();
            let (cfg, repo) = config_and_repo(&ui);
            let bak = expand(&bak_path);
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
                ui.set_log_text(
                    "backup not recognized (must be a .bak.* path shown by Apply)".into(),
                );
                return;
            };
            match store::restore_backup(&info.backup, &info.original) {
                Ok(r) => {
                    ui.set_log_text(
                        format!(
                            "restore: {} files back, leftovers (kept): {:?}",
                            r.restored, r.leftovers
                        )
                        .into(),
                    );
                }
                Err(e) => ui.set_log_text(format!("restore failed:\n{e}").into()),
            }
            refresh_all(&ui);
            if ui.get_download_mode() {
                show_repo_apps(&ui, &cfg, &repo);
            } else {
                show_app(
                    &ui,
                    &cfg,
                    &repo,
                    ui.get_active_app().as_str(),
                    &checks.borrow(),
                );
            }
        });
    }

    // Repository: fetch / pull / push / clone / init (autonomous).
    {
        let ui_handle = ui.as_weak();
        ui.on_repo_fetch(move || {
            let ui = ui_handle.unwrap();
            let repo = expand(&ui.get_repo_dir());
            match git::fetch(&repo) {
                Ok(_) => ui.set_log_text("checked for updates — status is vs remote now".into()),
                Err(e) => ui.set_log_text(format!("update check failed:\n{e}").into()),
            }
            refresh_repo(&ui);
        });
    }
    {
        let ui_handle = ui.as_weak();
        let checks = checks.clone();
        ui.on_repo_pull(move || {
            let ui = ui_handle.unwrap();
            let repo = expand(&ui.get_repo_dir());
            let (cfg, _) = config_and_repo(&ui);
            match git::pull_ff_only(&repo) {
                Ok(out) => {
                    ui.set_log_text(format!("pull ok:\n{out}").into());
                    refresh_all(&ui);
                    if ui.get_download_mode() {
                        show_repo_apps(&ui, &cfg, &repo);
                    } else {
                        show_app(
                            &ui,
                            &cfg,
                            &repo,
                            ui.get_active_app().as_str(),
                            &checks.borrow(),
                        );
                    }
                }
                Err(e) => ui.set_log_text(format!("pull failed:\n{e}").into()),
            }
            refresh_repo(&ui);
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_repo_push(move || {
            let ui = ui_handle.unwrap();
            let repo = expand(&ui.get_repo_dir());
            match git::push(&repo) {
                Ok(_) => ui.set_log_text("pushed".into()),
                Err(e) => ui.set_log_text(format!("push failed:\n{e}").into()),
            }
            refresh_repo(&ui);
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_repo_clone(move || {
            let ui = ui_handle.unwrap();
            let url = ui.get_remote_url().trim().to_string();
            let dest = expand(&ui.get_repo_dir());
            if url.is_empty() {
                ui.set_log_text("enter a GitHub URL first".into());
                return;
            }
            match git::clone_repo(&url, &dest) {
                Ok(_) => {
                    ui.set_log_text(format!("cloned {url} — switched to download mode").into());
                    let (cfg, _) = config_and_repo(&ui);
                    ui.set_download_mode(true);
                    show_repo_apps(&ui, &cfg, &dest);
                    refresh_all(&ui);
                }
                Err(e) => ui.set_log_text(format!("clone failed:\n{e}").into()),
            }
            refresh_repo(&ui);
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_repo_init(move || {
            let ui = ui_handle.unwrap();
            let (cfg, repo) = config_and_repo(&ui);
            let url = ui.get_remote_url().trim().to_string();
            if ui.get_repo_dir().trim().is_empty() {
                ui.set_log_text("set a local repo path first".into());
                return;
            }
            if let Err(e) = git::init_repo(&repo) {
                ui.set_log_text(format!("init failed:\n{e}").into());
                refresh_repo(&ui);
                return;
            }
            // Autonomous first snapshot: everything of the checked apps.
            let sel = selected_ids(&ui);
            let mut total = 0usize;
            let mut done = 0usize;
            for id in &sel {
                let app = ocs_core::apps::resolve_app(&cfg, id);
                let all: HashSet<PathBuf> = store::list_local_rels(&cfg, &app)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|f| f.rel)
                    .collect();
                match store::snapshot_selected(&cfg, &repo.join(&app.id), &app, &all) {
                    Ok(n) => {
                        total += n;
                        done += 1;
                    }
                    Err(e) => {
                        ui.set_log_text(format!("snapshot {id} failed:\n{e}").into());
                        refresh_repo(&ui);
                        return;
                    }
                }
            }
            let mut log = format!("initialized — {done} apps, {total} files snapshotted\n");
            match git::commit_all(&repo, "initial sync") {
                Ok(m) => log.push_str(&format!("commit: {m}\n")),
                Err(e) => log.push_str(&format!("commit failed: {e}\n")),
            }
            if url.is_empty() {
                // Fully autonomous: create the GitHub repo via gh, then publish.
                let name = repo
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("config-data")
                    .to_string();
                match git::gh_create_repo(&repo, &name)
                    .and_then(|_| git::push_upstream(&repo))
                {
                    Ok(_) => log.push_str("GitHub repo created + published"),
                    Err(e) => log.push_str(&format!(
                        "local repo ready, GitHub create failed:\n{e}\nTip: paste a URL above (or create the empty repo on github.com), then Init again"
                    )),
                }
            } else {
                match git::set_remote(&repo, &url).and_then(|_| git::push_upstream(&repo)) {
                    Ok(_) => log.push_str("remote attached + published to GitHub"),
                    Err(e) => log.push_str(&format!(
                        "local repo ready, publish failed (create the empty GitHub repo first?):\n{e}"
                    )),
                }
            }
            ui.set_log_text(log.into());
            refresh_all(&ui);
            refresh_repo(&ui);
        });
    }

    ui.run()
}
