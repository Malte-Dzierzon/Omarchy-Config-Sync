//! Read-only scanner over the config dir. No writes here.
//!
//! - One thread per app (std only, no extra deps).
//! - Never follows symlinks: no cycles, nothing outside HOME is pulled in.
//!   Broken links are counted, not fatal (cf. the dangling-link incident).
//! - Bounded: per-app file cap, unreadable entries are skipped + counted.

use crate::apps::AppSpec;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AppStatus {
    pub id: String,
    pub label: String,
    pub exists: bool,
    pub files: usize,
    pub bytes: u64,
    pub skipped: usize,
    pub broken_links: usize,
}

/// Resolve the config dir the Omarchy way: `$XDG_CONFIG_HOME`, else `~/.config`.
/// Pure (no env) so it stays unit-testable.
pub fn resolve_config_dir_with(home: Option<&Path>, xdg: Option<&Path>) -> PathBuf {
    if let Some(x) = xdg {
        if x.is_absolute() {
            return x.to_path_buf();
        }
    }
    match home {
        Some(h) => h.join(".config"),
        None => PathBuf::from(".config"),
    }
}

/// Env wrapper around [`resolve_config_dir_with`].
pub fn resolve_config_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    resolve_config_dir_with(home.as_deref(), xdg.as_deref())
}

fn empty_status(app: &AppSpec) -> AppStatus {
    AppStatus {
        id: app.id.clone(),
        label: app.label.clone(),
        exists: false,
        files: 0,
        bytes: 0,
        skipped: 0,
        broken_links: 0,
    }
}

fn scan_one(config_dir: &Path, app: &AppSpec) -> AppStatus {
    let mut st = empty_status(app);
    for rel in &app.rel_paths {
        let p = config_dir.join(rel);
        if p.exists() || is_broken_link(&p) {
            st.exists = true;
            let c = count_path(&p, &app.exclude_files);
            st.files += c.files;
            st.bytes += c.bytes;
            st.skipped += c.skipped;
            st.broken_links += c.broken;
        }
    }
    st
}

/// Status for each known app. Pure read-only, scanned in parallel.
/// Result order matches input order.
pub fn scan_apps(config_dir: &Path, apps: &[AppSpec]) -> Vec<AppStatus> {
    scan_apps_parallel(config_dir, apps)
}

pub fn scan_apps_parallel(config_dir: &Path, apps: &[AppSpec]) -> Vec<AppStatus> {
    if apps.is_empty() {
        return Vec::new();
    }
    // Bounded workers: one thread per app thrashes the disk once live
    // discovery finds dozens of entries — 8 chunks are plenty for dotfiles.
    // Order still matches the input order.
    let workers = apps.len().clamp(1, 8);
    let chunk = apps.len().div_ceil(workers);
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        let mut start = 0;
        while start < apps.len() {
            let end = (start + chunk).min(apps.len());
            let len = end - start;
            // Disjoint immutable slices — all borrows outlive the scope.
            let slice = &apps[start..end];
            handles.push((
                start,
                len,
                s.spawn(move || {
                    slice
                        .iter()
                        .map(|app| scan_one(config_dir, app))
                        .collect::<Vec<_>>()
                }),
            ));
            start = end;
        }
        let mut out = Vec::with_capacity(apps.len());
        for (start, len, h) in handles {
            match h.join() {
                Ok(mut v) => out.append(&mut v),
                Err(_) => out.extend(apps[start..start + len].iter().map(empty_status)),
            }
        }
        out
    })
}

/// Names of all other top-level entries in config dir (read-only, for the picker).
pub fn scan_other_entries(config_dir: &Path, known: &[AppSpec]) -> Vec<String> {
    let known_tops: Vec<&str> = known
        .iter()
        .flat_map(|a| a.rel_paths.iter())
        .map(|r| r.split('/').next().unwrap_or(r))
        .collect();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(config_dir) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !known_tops.contains(&name.as_str()) {
            out.push(name);
        }
    }
    out.sort();
    out
}

#[derive(Default)]
struct Counts {
    files: usize,
    bytes: u64,
    skipped: usize,
    broken: usize,
}

fn count_path(path: &Path, exclude: &[String]) -> Counts {
    let mut c = Counts::default();
    if is_excluded(path, exclude) {
        c.skipped += 1;
        return c;
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => {
            c.skipped += 1;
            return c;
        }
    };
    if meta.is_symlink() {
        // Never follow: avoids cycles and escaping HOME. Just classify.
        if path.exists() {
            c.skipped += 1;
        } else {
            c.broken += 1;
        }
        return c;
    }
    if meta.is_file() {
        c.files += 1;
        c.bytes += meta.len();
        return c;
    }
    if !meta.is_dir() {
        c.skipped += 1;
        return c;
    }
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            c.skipped += 1; // permission denied etc.: skip dir, keep going
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if is_excluded(&p, exclude) {
                c.skipped += 1;
                continue;
            }
            let m = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => {
                    c.skipped += 1;
                    continue;
                }
            };
            if m.is_symlink() {
                if p.exists() {
                    c.skipped += 1;
                } else {
                    c.broken += 1;
                }
            } else if m.is_dir() {
                stack.push(p);
            } else if m.is_file() {
                c.files += 1;
                c.bytes += m.len();
                if c.files > 20_000 {
                    return c;
                }
            } else {
                c.skipped += 1;
            }
        }
    }
    c
}

fn is_broken_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.is_symlink())
        .unwrap_or(false)
        && !p.exists()
}

fn is_excluded(path: &Path, exclude: &[String]) -> bool {
    if exclude.is_empty() {
        return false;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| exclude.iter().any(|e| e == n))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_xdg_then_home() {
        let out =
            resolve_config_dir_with(Some(Path::new("/home/u")), Some(Path::new("/home/u/.cfg")));
        assert_eq!(out, PathBuf::from("/home/u/.cfg"));
        let out = resolve_config_dir_with(Some(Path::new("/home/u")), None);
        assert_eq!(out, PathBuf::from("/home/u/.config"));
        // relative XDG is ignored (must be absolute)
        let out = resolve_config_dir_with(Some(Path::new("/home/u")), Some(Path::new("rel")));
        assert_eq!(out, PathBuf::from("/home/u/.config"));
    }

    #[test]
    fn broken_and_loop_links_neither_hang_nor_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("c");
        std::fs::create_dir_all(cfg.join("zed")).unwrap();
        std::fs::write(cfg.join("zed/s.json"), "{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("nope.json", cfg.join("zed/broken.json")).unwrap();
            symlink(cfg.join("zed"), cfg.join("zed/loop")).unwrap();
        }
        let apps = crate::builtin_apps();
        let st = scan_apps_parallel(&cfg, &apps);
        assert_eq!(st.len(), apps.len()); // order + count preserved
        assert_eq!(st[0].id, apps[0].id);
        let zed = st.iter().find(|s| s.id == "zed").unwrap();
        assert!(zed.exists);
        assert_eq!(zed.files, 1);
        #[cfg(unix)]
        {
            assert_eq!(zed.broken_links, 1);
            assert!(zed.skipped >= 1); // the dir loop is not followed
        }
    }
}
