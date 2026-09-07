//! Copy-only snapshot + backup-apply + restore. NEVER deletes user data.
//!
//! Invariants:
//! - push: copy config -> repo/<app>/ (overwrite files, never delete extras in repo)
//! - preview: list what pull/apply WOULD change (shared plan, apply must reuse it)
//! - apply: backup existing target to `<target>.bak.<epoch>` (rename = move, no loss),
//!   then copy in. If backup fails, abort before writing anything.
//! - restore: copy a `.bak.*` back over its original, file by file, never deleting.
//!   Files missing from the backup are left untouched and reported as leftovers.
//! - symlinks are recreated as symlinks and never followed (no cycles, nothing
//!   outside HOME is pulled in or overwritten).

use crate::apps::AppSpec;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpKind {
    New,
    Overwrite,
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct FileOp {
    /// Path relative to the app dir, e.g. "keymap.json".
    pub rel: PathBuf,
    pub kind: OpKind,
    pub bytes: u64,
}

/// Build the shared preview plan: compare repo/<app> against config.
/// Caller must pass the SAME plan to `apply_plan` (never rebuild in between).
pub fn preview(config_dir: &Path, repo_app_dir: &Path, app: &AppSpec) -> io::Result<Vec<FileOp>> {
    let mut ops = Vec::new();
    let staged = collect_repo_files(repo_app_dir)?;
    for (rel, repo_file) in staged {
        if is_excluded_rel(&rel, &app.exclude_files) {
            continue;
        }
        let target = target_for_rel(config_dir, app, &rel);
        let kind = if !target.exists() {
            OpKind::New
        } else if files_differ(&repo_file, &target)? {
            OpKind::Overwrite
        } else {
            OpKind::Unchanged
        };
        let bytes = repo_file.metadata().map(|m| m.len()).unwrap_or(0);
        ops.push(FileOp { rel, kind, bytes });
    }
    ops.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(ops)
}

/// Snapshot config -> repo/<app>/ (copy-only). Writes manifest.json alongside.
pub fn snapshot_to_repo(
    config_dir: &Path,
    repo_app_dir: &Path,
    app: &AppSpec,
) -> io::Result<usize> {
    let all: std::collections::HashSet<PathBuf> = list_local_rels(config_dir, app)?
        .into_iter()
        .map(|f| f.rel)
        .collect();
    snapshot_selected(config_dir, repo_app_dir, app, &all)
}

/// Snapshot only the selected repo-layout files. Writes manifest.json alongside.
pub fn snapshot_selected(
    config_dir: &Path,
    repo_app_dir: &Path,
    app: &AppSpec,
    selected: &std::collections::HashSet<PathBuf>,
) -> io::Result<usize> {
    let mut copied = 0usize;
    for rel in &app.rel_paths {
        let src = config_dir.join(rel);
        let meta = match std::fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(_) => continue, // missing/unreadable root: skip, never abort
        };
        if meta.is_symlink() || meta.is_file() {
            if is_excluded(&src, &app.exclude_files) {
                continue;
            }
            let repo_rel = PathBuf::from(src.file_name().unwrap_or_default());
            if selected.contains(&repo_rel) {
                std::fs::create_dir_all(repo_app_dir)?;
                copy_file(&src, &repo_app_dir.join(&repo_rel))?;
                copied += 1;
            }
        } else if meta.is_dir() {
            let mut stack = vec![src.clone()];
            while let Some(dir) = stack.pop() {
                for e in std::fs::read_dir(&dir)?.flatten() {
                    let p = e.path();
                    if is_excluded(&p, &app.exclude_files) {
                        continue;
                    }
                    let m = match std::fs::symlink_metadata(&p) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    if m.is_dir() && !m.is_symlink() {
                        stack.push(p);
                    } else if m.is_file() || m.is_symlink() {
                        let repo_rel = p.strip_prefix(&src).unwrap_or(&p).to_path_buf();
                        if selected.contains(&repo_rel) {
                            copy_file(&p, &repo_app_dir.join(&repo_rel))?;
                            copied += 1;
                        }
                    }
                }
            }
        }
    }
    write_manifest(repo_app_dir, &app.id, copied)?;
    Ok(copied)
}

#[derive(Debug, Clone)]
pub struct SelectableFile {
    /// Repo-layout relative path, e.g. `keymap.json`.
    pub rel: PathBuf,
    pub bytes: u64,
    pub is_link: bool,
}

/// All syncable local files of an app in repo layout (excludes applied).
/// Sorted. For the per-file checklist.
pub fn list_local_rels(config_dir: &Path, app: &AppSpec) -> io::Result<Vec<SelectableFile>> {
    let mut out = Vec::new();
    for rel in &app.rel_paths {
        let src = config_dir.join(rel);
        let meta = match std::fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_symlink() || meta.is_file() {
            if is_excluded(&src, &app.exclude_files) {
                continue;
            }
            out.push(SelectableFile {
                rel: PathBuf::from(src.file_name().unwrap_or_default()),
                bytes: if meta.is_symlink() { 0 } else { meta.len() },
                is_link: meta.is_symlink(),
            });
        } else if meta.is_dir() {
            let mut stack = vec![src.clone()];
            while let Some(dir) = stack.pop() {
                for e in std::fs::read_dir(&dir)?.flatten() {
                    let p = e.path();
                    if is_excluded(&p, &app.exclude_files) {
                        continue;
                    }
                    let m = match std::fs::symlink_metadata(&p) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    if m.is_dir() && !m.is_symlink() {
                        stack.push(p);
                    } else if m.is_file() || m.is_symlink() {
                        out.push(SelectableFile {
                            rel: p.strip_prefix(&src).unwrap_or(&p).to_path_buf(),
                            bytes: if m.is_symlink() { 0 } else { m.len() },
                            is_link: m.is_symlink(),
                        });
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Synced,
    Modified,
    NewLocal,
    MissingLocal,
}

#[derive(Debug, Clone)]
pub struct FileCompare {
    pub rel: PathBuf,
    pub state: FileState,
    pub bytes: u64,
}

/// Local-vs-repo comparison over the union of both sides (excludes applied).
/// `selected` filters repo-layout rels when `Some`. Never aborts on single
/// unreadable files — those read as `Modified`.
pub fn compare(
    config_dir: &Path,
    repo_app_dir: &Path,
    app: &AppSpec,
    selected: Option<&[PathBuf]>,
) -> io::Result<Vec<FileCompare>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let keep = |rel: &Path| {
        selected
            .map(|s| s.iter().any(|x| x.as_path() == rel))
            .unwrap_or(true)
    };
    for f in list_local_rels(config_dir, app)? {
        if !keep(&f.rel) {
            continue;
        }
        seen.insert(f.rel.clone());
        let repo_file = repo_app_dir.join(&f.rel);
        let state = if exists_any(&repo_file) {
            let target = target_for_rel(config_dir, app, &f.rel);
            match files_differ(&repo_file, &target) {
                Ok(false) => FileState::Synced,
                _ => FileState::Modified,
            }
        } else {
            FileState::NewLocal
        };
        out.push(FileCompare {
            rel: f.rel,
            state,
            bytes: f.bytes,
        });
    }
    for (rel, repo_file) in collect_repo_files(repo_app_dir)? {
        if seen.contains(&rel) || is_excluded_rel(&rel, &app.exclude_files) || !keep(&rel) {
            continue;
        }
        out.push(FileCompare {
            bytes: repo_file.symlink_metadata().map(|m| m.len()).unwrap_or(0),
            rel,
            state: FileState::MissingLocal,
        });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

fn exists_any(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

#[derive(Debug, Clone)]
pub struct RepoApp {
    pub id: String,
    pub files: usize,
    pub bytes: u64,
}

/// Apps present in a cloned repo: subdirs holding files (besides manifest).
/// Sorted by id. For the download picker.
pub fn list_repo_apps(repo: &Path) -> Vec<RepoApp> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(repo) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if p.file_name().and_then(|n| n.to_str()) == Some(".git") {
            continue;
        }
        let staged = collect_repo_files(&p).unwrap_or_default();
        if staged.is_empty() {
            continue;
        }
        let bytes = staged
            .iter()
            .map(|(_, f)| f.symlink_metadata().map(|m| m.len()).unwrap_or(0))
            .sum();
        out.push(RepoApp {
            id: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            files: staged.len(),
            bytes,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Apply a preview plan: backup each affected existing target, then copy.
/// Backup = rename `<target>` to `<target>.bak.<epoch>`; aborts on backup failure.
pub fn apply_plan(
    config_dir: &Path,
    repo_app_dir: &Path,
    app: &AppSpec,
    plan: &[FileOp],
) -> io::Result<ApplyReport> {
    let mut report = ApplyReport {
        app: app.id.clone(),
        backed_up: Vec::new(),
        written: 0,
        unchanged: 0,
        merged_back: 0,
    };
    // 1. Backup every existing top-level target this app touches (once each).
    for (bak, _) in backup_targets(config_dir, app)? {
        report.backed_up.push(bak.display().to_string());
    }
    // 2. Copy per plan (targets were just moved, so write all).
    for op in plan {
        let src = repo_app_dir.join(&op.rel);
        let dst = target_for_rel(config_dir, app, &op.rel);
        copy_file(&src, &dst)?;
        match op.kind {
            OpKind::Unchanged => report.unchanged += 1,
            OpKind::New | OpKind::Overwrite => report.written += 1,
        }
    }
    Ok(report)
}

/// Apply only the selected repo-layout files. Unselected live files (and
/// machine-local files like `monitors.lua`) are merged back from the backup,
/// so a partial apply can never strand or lose them.
pub fn apply_plan_selected(
    config_dir: &Path,
    repo_app_dir: &Path,
    app: &AppSpec,
    plan: &[FileOp],
    selected: &std::collections::HashSet<PathBuf>,
) -> io::Result<ApplyReport> {
    let mut report = ApplyReport {
        app: app.id.clone(),
        backed_up: Vec::new(),
        written: 0,
        unchanged: 0,
        merged_back: 0,
    };
    // 1. Same whole-target backup as a full apply (rename = no loss).
    let pairs = backup_targets(config_dir, app)?;
    for (bak, _) in &pairs {
        report.backed_up.push(bak.display().to_string());
    }
    // 2. Selected files from the repo.
    for op in plan {
        if !selected.contains(&op.rel) {
            continue;
        }
        let src = repo_app_dir.join(&op.rel);
        let dst = target_for_rel(config_dir, app, &op.rel);
        copy_file(&src, &dst)?;
        match op.kind {
            OpKind::Unchanged => report.unchanged += 1,
            OpKind::New | OpKind::Overwrite => report.written += 1,
        }
    }
    // 3. Merge back everything not selected (fail-safe: unknown entries restore).
    for (bak, original) in &pairs {
        report.merged_back += merge_back_selected(bak, original, selected)?;
    }
    Ok(report)
}

/// Rename each existing top-level target to `<target>.bak.<epoch>`.
/// Returns `(backup, original)` pairs. Aborts on the first backup failure
/// before anything is written.
fn backup_targets(config_dir: &Path, app: &AppSpec) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut pairs = Vec::new();
    for rel in &app.rel_paths {
        let target = config_dir.join(rel);
        if target.exists() {
            let bak = PathBuf::from(format!("{}.bak.{}", target.display(), epoch));
            if !bak.exists() {
                std::fs::rename(&target, &bak).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("backup failed for {}: {e}", target.display()),
                    )
                })?;
            }
            pairs.push((bak, target));
        }
    }
    Ok(pairs)
}

/// Copy backup content back except the selected repo-layout rels.
/// Returns the number of files merged back.
fn merge_back_selected(
    backup: &Path,
    original: &Path,
    selected: &std::collections::HashSet<PathBuf>,
) -> io::Result<usize> {
    let bmeta = std::fs::symlink_metadata(backup)?;
    // File backup (e.g. `shell.json.bak.<epoch>`): repo rel is the base name.
    if bmeta.is_file() || bmeta.is_symlink() {
        let base = backup
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .split(".bak.")
            .next()
            .unwrap_or("");
        let repo_rel = PathBuf::from(base);
        if base.is_empty() || !selected.contains(&repo_rel) {
            copy_file(backup, original)?;
            return Ok(1);
        }
        return Ok(0);
    }
    if !bmeta.is_dir() {
        return Ok(0);
    }
    // Dir backup mirrors repo layout: backup-rel == repo-rel.
    let mut merged = 0usize;
    let mut stack = vec![backup.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)?.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(backup).unwrap_or(&p).to_path_buf();
            let m = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if m.is_dir() && !m.is_symlink() {
                std::fs::create_dir_all(original.join(&rel))?;
                stack.push(p);
            } else if (m.is_file() || m.is_symlink()) && !selected.contains(&rel) {
                copy_file(&p, &original.join(&rel))?;
                merged += 1;
            }
        }
    }
    Ok(merged)
}

#[derive(Debug, Clone)]
pub struct ApplyReport {
    pub app: String,
    pub backed_up: Vec<String>,
    pub written: usize,
    pub unchanged: usize,
    pub merged_back: usize,
}

#[derive(Debug, Clone)]
pub struct BackupInfo {
    pub backup: PathBuf,
    pub original: PathBuf,
}

/// Find `<name>.bak.*` backups for an app's top-level targets. Read-only.
pub fn list_backups(config_dir: &Path, app: &AppSpec) -> Vec<BackupInfo> {
    let mut out = Vec::new();
    for rel in &app.rel_paths {
        let target = config_dir.join(rel);
        let (Some(parent), Some(base)) =
            (target.parent(), target.file_name().and_then(|n| n.to_str()))
        else {
            continue;
        };
        let prefix = format!("{base}.bak.");
        let Ok(rd) = std::fs::read_dir(parent) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                out.push(BackupInfo {
                    backup: e.path(),
                    original: target.clone(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.backup.cmp(&b.backup));
    out
}

#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub restored: usize,
    pub leftovers: Vec<String>,
}

/// Copy a backup back over its original, file by file. NEVER deletes:
/// files that exist in the current config but not in the backup are left
/// untouched and reported as `leftovers` for manual review.
pub fn restore_backup(backup: &Path, original: &Path) -> io::Result<RestoreReport> {
    let bmeta = std::fs::symlink_metadata(backup)?;
    if bmeta.is_file() || bmeta.is_symlink() {
        copy_file(backup, original)?;
        return Ok(RestoreReport {
            restored: 1,
            leftovers: Vec::new(),
        });
    }
    if !bmeta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "backup is neither file nor dir",
        ));
    }
    let mut report = RestoreReport {
        restored: 0,
        leftovers: Vec::new(),
    };
    // 1. Copy every backup entry back over the original location.
    let mut stack = vec![backup.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)?.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(backup).unwrap_or(&p).to_path_buf();
            let m = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if m.is_dir() && !m.is_symlink() {
                std::fs::create_dir_all(original.join(&rel))?;
                stack.push(p);
            } else if m.is_file() || m.is_symlink() {
                copy_file(&p, &original.join(&rel))?;
                report.restored += 1;
            }
        }
    }
    // 2. Report current files missing from the backup (left untouched).
    if original.exists() {
        let mut stack = vec![original.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                let rel = p.strip_prefix(original).unwrap_or(&p).to_path_buf();
                let m = match std::fs::symlink_metadata(&p) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if m.is_dir() && !m.is_symlink() {
                    stack.push(p);
                } else if std::fs::symlink_metadata(backup.join(&rel)).is_err() {
                    report.leftovers.push(rel.display().to_string());
                }
            }
        }
        report.leftovers.sort();
    }
    Ok(report)
}

// --- helpers (all copy-only, no removes) ---

fn target_for_rel(config_dir: &Path, app: &AppSpec, rel: &Path) -> PathBuf {
    // repo layout mirrors the FIRST matching rel root.
    // zed: repo "keymap.json" -> ~/.config/zed/keymap.json
    // omarchy-shell: repo "shell.json" -> ~/.config/omarchy/shell.json
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    for root in &app.rel_paths {
        let root_file = Path::new(root).file_name().and_then(|n| n.to_str());
        if let Some(f) = root_file {
            if rel_str == f || rel_str.starts_with(&format!("{f}/")) {
                // map back under the root's parent
                let parent = Path::new(root).parent().unwrap_or(Path::new(""));
                return config_dir.join(parent).join(&rel_str);
            }
        }
        // single-dir roots like "zed"/"hypr": repo files sit at app root
        if !root.contains('/') {
            return config_dir.join(root).join(&rel_str);
        }
    }
    config_dir.join(&rel_str)
}

fn collect_repo_files(repo_app_dir: &Path) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let mut out = Vec::new();
    if !repo_app_dir.exists() {
        return Ok(out);
    }
    let mut stack = vec![repo_app_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)?.flatten() {
            let p = e.path();
            // Never follow symlinks: descend into real dirs only.
            let m = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue, // unreadable: skip, never abort the whole preview
            };
            if m.is_dir() && !m.is_symlink() {
                stack.push(p);
            } else if m.is_file() || m.is_symlink() {
                if p.file_name().and_then(|n| n.to_str()) == Some("manifest.json") {
                    continue;
                }
                let rel = p.strip_prefix(repo_app_dir).unwrap_or(&p).to_path_buf();
                out.push((rel, p));
            }
        }
    }
    Ok(out)
}

fn files_differ(a: &Path, b: &Path) -> io::Result<bool> {
    let ma = std::fs::symlink_metadata(a)?;
    let mb = std::fs::symlink_metadata(b)?;
    if ma.is_symlink() || mb.is_symlink() {
        if !ma.is_symlink() || !mb.is_symlink() {
            return Ok(true);
        }
        return Ok(std::fs::read_link(a)? != std::fs::read_link(b)?);
    }
    if ma.len() != mb.len() {
        return Ok(true);
    }
    Ok(std::fs::read(a)? != std::fs::read(b)?)
}

/// Copy one file or symlink. Creates parent dirs. Overwrites the exact
/// destination file only — directories are never replaced or removed.
/// Symlinks are recreated as symlinks (never followed).
fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_symlink() {
        let target = std::fs::read_link(src)?;
        // Remove only a file/symlink we are about to replace — never a dir.
        if let Ok(dm) = std::fs::symlink_metadata(dst) {
            if dm.is_dir() && !dm.is_symlink() {
                return Err(io::Error::other(format!(
                    "refusing to replace directory {}",
                    dst.display()
                )));
            }
            std::fs::remove_file(dst)?;
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, dst)?;
        }
        #[cfg(not(unix))]
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "symlink copy needs unix",
            ));
        }
        return Ok(());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        return Ok(());
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

fn is_excluded(path: &Path, exclude: &[String]) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| exclude.iter().any(|e| e == n))
        .unwrap_or(false)
}

fn is_excluded_rel(rel: &Path, exclude: &[String]) -> bool {
    rel.components().any(|c| {
        c.as_os_str()
            .to_str()
            .map(|n| exclude.iter().any(|e| e == n))
            .unwrap_or(false)
    })
}

fn write_manifest(repo_app_dir: &Path, app_id: &str, files: usize) -> io::Result<()> {
    std::fs::create_dir_all(repo_app_dir)?;
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = serde_json::json!({
        "app": app_id,
        "files": files,
        "pushed_at_epoch": epoch,
        "schema_version": 1,
    });
    std::fs::write(
        repo_app_dir.join("manifest.json"),
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::find_app;

    fn write(p: &Path, content: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn hypr_excludes_machine_local() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        write(&cfg.join("hypr/monitors.lua"), "local");
        write(&cfg.join("hypr/hyprland.conf"), "ok");
        let app = find_app("hypr").unwrap();
        let repo = tmp.path().join("repo/hypr");
        let n = snapshot_to_repo(&cfg, &repo, &app).unwrap();
        assert_eq!(n, 1);
        assert!(!repo.join("monitors.lua").exists());
        assert!(repo.join("hyprland.conf").exists());
    }

    #[test]
    fn apply_backs_up_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let repo = tmp.path().join("repo/zed");
        write(&cfg.join("zed/settings.json"), "old");
        write(&repo.join("settings.json"), "new");
        let app = find_app("zed").unwrap();
        let plan = preview(&cfg, &repo, &app).unwrap();
        assert!(plan.iter().any(|o| o.kind == OpKind::Overwrite));
        let report = apply_plan(&cfg, &repo, &app, &plan).unwrap();
        assert_eq!(report.written, 1);
        assert_eq!(report.backed_up.len(), 1);
        // backup kept the old content, target has the new one
        let bak = PathBuf::from(&report.backed_up[0]);
        assert_eq!(
            std::fs::read_to_string(bak.join("settings.json")).unwrap(),
            "old"
        );
        assert_eq!(
            std::fs::read_to_string(cfg.join("zed/settings.json")).unwrap(),
            "new"
        );
    }

    #[test]
    fn apply_then_preview_is_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let repo = tmp.path().join("repo/zed");
        write(&repo.join("keymap.json"), "keys");
        let app = find_app("zed").unwrap();
        let plan = preview(&cfg, &repo, &app).unwrap();
        apply_plan(&cfg, &repo, &app, &plan).unwrap();
        let again = preview(&cfg, &repo, &app).unwrap();
        assert!(again.iter().all(|o| o.kind == OpKind::Unchanged));
    }

    #[test]
    fn restore_brings_back_backup_without_deleting_new_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let repo = tmp.path().join("repo/zed");
        write(&cfg.join("zed/settings.json"), "old");
        write(&repo.join("settings.json"), "new");
        let app = find_app("zed").unwrap();
        let plan = preview(&cfg, &repo, &app).unwrap();
        let report = apply_plan(&cfg, &repo, &app, &plan).unwrap();
        // user adds a new file after apply
        write(&cfg.join("zed/extra.json"), "extra");
        let bak = PathBuf::from(&report.backed_up[0]);
        let rr = restore_backup(&bak, &cfg.join("zed")).unwrap();
        assert_eq!(
            std::fs::read_to_string(cfg.join("zed/settings.json")).unwrap(),
            "old"
        );
        // new file untouched, reported as leftover
        assert!(cfg.join("zed/extra.json").exists());
        assert_eq!(rr.leftovers, vec!["extra.json".to_string()]);
        // backups are discoverable
        assert_eq!(list_backups(&cfg, &app).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_survive_snapshot_and_apply_as_links() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let repo = tmp.path().join("repo/zed");
        std::fs::create_dir_all(cfg.join("zed")).unwrap();
        write(&cfg.join("zed/real.json"), "{}");
        symlink("real.json", cfg.join("zed/link.json")).unwrap();
        let app = find_app("zed").unwrap();
        let n = snapshot_to_repo(&cfg, &repo, &app).unwrap();
        assert_eq!(n, 2);
        // stored as link, not followed
        assert!(std::fs::symlink_metadata(repo.join("link.json"))
            .unwrap()
            .is_symlink());
        // apply onto empty dir recreates the link
        let cfg2 = tmp.path().join("config2");
        let plan = preview(&cfg2, &repo, &app).unwrap();
        apply_plan(&cfg2, &repo, &app, &plan).unwrap();
        assert_eq!(
            std::fs::read_link(cfg2.join("zed/link.json")).unwrap(),
            PathBuf::from("real.json")
        );
    }

    #[test]
    fn compare_reports_all_four_states() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let repo = tmp.path().join("repo/zed");
        write(&cfg.join("zed/same.json"), "x");
        write(&cfg.join("zed/changed.json"), "local");
        write(&cfg.join("zed/only-local.json"), "new");
        write(&repo.join("same.json"), "x");
        write(&repo.join("changed.json"), "repo");
        write(&repo.join("only-repo.json"), "gone");
        write(&repo.join("manifest.json"), "{}");
        let app = find_app("zed").unwrap();
        let cmp = compare(&cfg, &repo, &app, None).unwrap();
        let state = |n: &str| {
            cmp.iter()
                .find(|c| c.rel.as_path() == Path::new(n))
                .map(|c| c.state)
        };
        assert_eq!(state("same.json"), Some(FileState::Synced));
        assert_eq!(state("changed.json"), Some(FileState::Modified));
        assert_eq!(state("only-local.json"), Some(FileState::NewLocal));
        assert_eq!(state("only-repo.json"), Some(FileState::MissingLocal));
        assert_eq!(state("manifest.json"), None); // never synced
    }

    #[test]
    fn selective_snapshot_and_apply_keep_unselected_files() {
        use std::collections::HashSet;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let repo = tmp.path().join("repo/zed");
        write(&cfg.join("zed/a.json"), "a1");
        write(&cfg.join("zed/b.json"), "b1");
        let app = find_app("zed").unwrap();
        // push only a.json
        let sel: HashSet<PathBuf> = [PathBuf::from("a.json")].into();
        assert_eq!(snapshot_selected(&cfg, &repo, &app, &sel).unwrap(), 1);
        assert!(repo.join("a.json").exists());
        assert!(!repo.join("b.json").exists());
        // repo moves on for a.json only
        write(&repo.join("a.json"), "a2");
        let plan = preview(&cfg, &repo, &app).unwrap();
        let r = apply_plan_selected(&cfg, &repo, &app, &plan, &sel).unwrap();
        assert_eq!(r.written, 1);
        assert_eq!(
            std::fs::read_to_string(cfg.join("zed/a.json")).unwrap(),
            "a2"
        );
        // b.json survived via merge-back, byte-identical
        assert_eq!(
            std::fs::read_to_string(cfg.join("zed/b.json")).unwrap(),
            "b1"
        );
        assert!(r.merged_back >= 1);
        // machine-local files survive selective apply too
        let app_hypr = find_app("hypr").unwrap();
        write(&cfg.join("hypr/hyprland.conf"), "conf1");
        write(&cfg.join("hypr/monitors.lua"), "local-mon");
        write(&repo.join("hyprland.conf"), "conf2");
        let repo_h = tmp.path().join("repo/hypr");
        std::fs::create_dir_all(&repo_h).unwrap();
        std::fs::rename(repo.join("hyprland.conf"), repo_h.join("hyprland.conf")).unwrap();
        let sel_h: HashSet<PathBuf> = [PathBuf::from("hyprland.conf")].into();
        let plan_h = preview(&cfg, &repo_h, &app_hypr).unwrap();
        apply_plan_selected(&cfg, &repo_h, &app_hypr, &plan_h, &sel_h).unwrap();
        assert_eq!(
            std::fs::read_to_string(cfg.join("hypr/monitors.lua")).unwrap(),
            "local-mon"
        );
    }
}
