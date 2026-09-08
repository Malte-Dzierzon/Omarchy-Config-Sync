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

/// Budget caps: sync stays fast and repos stay small no matter which app
/// (or how bloated its cache dirs) is selected. Skipped files are counted
/// in [`SkipStats`] and surfaced — never silently dragged along.
pub const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_FILES_PER_APP: usize = 20_000;

/// Files skipped while walking, for honest UI counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkipStats {
    /// Over [`MAX_FILE_BYTES`] (caches, blobs, datasets).
    pub large: usize,
    /// Vanished or permission-denied mid-walk.
    pub unreadable: usize,
    /// Hit [`MAX_FILES_PER_APP`]; the walk stopped early.
    pub truncated: bool,
}

impl SkipStats {
    pub fn add(&mut self, o: &SkipStats) {
        self.large += o.large;
        self.unreadable += o.unreadable;
        self.truncated = self.truncated || o.truncated;
    }

    pub fn total(&self) -> usize {
        self.large + self.unreadable
    }
}

/// Outcome of [`snapshot_selected`]: what landed in the repo vs. what was
/// deliberately left out (see [`SkipStats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotReport {
    pub copied: usize,
    pub skipped: SkipStats,
}

/// Snapshot config -> repo/<app>/ (copy-only). Writes manifest.json alongside.
pub fn snapshot_to_repo(
    config_dir: &Path,
    repo_app_dir: &Path,
    app: &AppSpec,
) -> io::Result<SnapshotReport> {
    let all: std::collections::HashSet<PathBuf> = list_local_rels(config_dir, app)?
        .0
        .into_iter()
        .map(|f| f.rel)
        .collect();
    snapshot_selected(config_dir, repo_app_dir, app, &all)
}

/// Snapshot only the selected repo-layout files. Writes manifest.json alongside.
/// Files over [`MAX_FILE_BYTES`] are skipped (counted, never copied); a
/// permission-denied subdir skips just that dir instead of aborting the app.
pub fn snapshot_selected(
    config_dir: &Path,
    repo_app_dir: &Path,
    app: &AppSpec,
    selected: &std::collections::HashSet<PathBuf>,
) -> io::Result<SnapshotReport> {
    let mut report = SnapshotReport::default();
    if selected.is_empty() {
        return Ok(report); // nothing to find: skip giant walks entirely
    }
    // Walk budget: even finding selected files inside a giant tree must end.
    // `seen` counts every usable entry (selected or not); copies stop with
    // `truncated` set instead of wandering forever.
    let mut seen = 0usize;
    // Local helper: cap + exclusion in one place so both walk arms agree.
    let usable = |p: &Path, m: &std::fs::Metadata, stats: &mut SkipStats| -> bool {
        if is_excluded(p, &app.exclude_files) {
            return false;
        }
        if m.is_file() && m.len() > MAX_FILE_BYTES {
            stats.large += 1;
            return false;
        }
        true
    };
    for rel in &app.rel_paths {
        let src = config_dir.join(rel);
        let meta = match std::fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(_) => {
                report.skipped.unreadable += 1;
                continue; // missing/unreadable root: skip, never abort
            }
        };
        if meta.is_symlink() || meta.is_file() {
            if !usable(&src, &meta, &mut report.skipped) {
                continue;
            }
            let repo_rel = PathBuf::from(src.file_name().unwrap_or_default());
            if selected.contains(&repo_rel) {
                std::fs::create_dir_all(repo_app_dir)?;
                copy_file(&src, &repo_app_dir.join(&repo_rel))?;
                report.copied += 1;
            }
        } else if meta.is_dir() {
            let mut stack = vec![src.clone()];
            while let Some(dir) = stack.pop() {
                let rd = match std::fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(_) => {
                        report.skipped.unreadable += 1;
                        continue;
                    }
                };
                for e in rd.flatten() {
                    let p = e.path();
                    let m = match std::fs::symlink_metadata(&p) {
                        Ok(m) => m,
                        Err(_) => {
                            report.skipped.unreadable += 1;
                            continue;
                        }
                    };
                    if !usable(&p, &m, &mut report.skipped) {
                        continue;
                    }
                    if m.is_dir() && !m.is_symlink() {
                        stack.push(p);
                    } else if m.is_file() || m.is_symlink() {
                        seen += 1;
                        if seen > MAX_FILES_PER_APP {
                            report.skipped.truncated = true;
                            write_manifest(repo_app_dir, &app.id, report.copied)?;
                            return Ok(report);
                        }
                        let repo_rel = p.strip_prefix(&src).unwrap_or(&p).to_path_buf();
                        if selected.contains(&repo_rel) {
                            copy_file(&p, &repo_app_dir.join(&repo_rel))?;
                            report.copied += 1;
                        }
                    }
                }
            }
        }
    }
    write_manifest(repo_app_dir, &app.id, report.copied)?;
    Ok(report)
}

#[derive(Debug, Clone)]
pub struct SelectableFile {
    /// Repo-layout relative path, e.g. `keymap.json`.
    pub rel: PathBuf,
    pub bytes: u64,
    pub is_link: bool,
}

/// All syncable local files of an app in repo layout (excludes applied).
/// Sorted. For the per-file checklist. Over-`MAX_FILE_BYTES` files and the
/// tail past `MAX_FILES_PER_APP` are reported in [`SkipStats`], never listed.
pub fn list_local_rels(
    config_dir: &Path,
    app: &AppSpec,
) -> io::Result<(Vec<SelectableFile>, SkipStats)> {
    let mut out = Vec::new();
    let mut skipped = SkipStats::default();
    for rel in &app.rel_paths {
        if skipped.truncated {
            break;
        }
        let src = config_dir.join(rel);
        let meta = match std::fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(_) => {
                skipped.unreadable += 1;
                continue;
            }
        };
        if meta.is_symlink() || meta.is_file() {
            if is_excluded(&src, &app.exclude_files) {
                continue;
            }
            push_capped(
                &mut out,
                &mut skipped,
                PathBuf::from(src.file_name().unwrap_or_default()),
                if meta.is_symlink() { 0 } else { meta.len() },
                meta.is_symlink(),
            );
        } else if meta.is_dir() {
            let mut stack = vec![src.clone()];
            while let Some(dir) = stack.pop() {
                if skipped.truncated {
                    break;
                }
                let rd = match std::fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(_) => {
                        skipped.unreadable += 1;
                        continue;
                    }
                };
                for e in rd.flatten() {
                    let p = e.path();
                    if is_excluded(&p, &app.exclude_files) {
                        continue;
                    }
                    let m = match std::fs::symlink_metadata(&p) {
                        Ok(m) => m,
                        Err(_) => {
                            skipped.unreadable += 1;
                            continue;
                        }
                    };
                    if m.is_dir() && !m.is_symlink() {
                        stack.push(p);
                    } else if m.is_file() || m.is_symlink() {
                        push_capped(
                            &mut out,
                            &mut skipped,
                            p.strip_prefix(&src).unwrap_or(&p).to_path_buf(),
                            if m.is_symlink() { 0 } else { m.len() },
                            m.is_symlink(),
                        );
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok((out, skipped))
}

/// Push one entry with budget enforcement (see [`list_local_rels`]).
/// Free function (not a closure) so loop conditions can read the stats.
fn push_capped(
    out: &mut Vec<SelectableFile>,
    skipped: &mut SkipStats,
    rel: PathBuf,
    bytes: u64,
    is_link: bool,
) {
    if !is_link && bytes > MAX_FILE_BYTES {
        skipped.large += 1;
        return;
    }
    if out.len() >= MAX_FILES_PER_APP {
        skipped.truncated = true;
        return;
    }
    out.push(SelectableFile {
        rel,
        bytes,
        is_link,
    });
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
/// unreadable files — those read as `Modified`. Over-`MAX_FILE_BYTES` files
/// compare by size only (documented heuristic: reading GBs on every refresh
/// costs more than the rare same-size change it could miss); everything
/// skipped is reported in the returned [`SkipStats`].
pub fn compare(
    config_dir: &Path,
    repo_app_dir: &Path,
    app: &AppSpec,
    selected: Option<&[PathBuf]>,
) -> io::Result<(Vec<FileCompare>, SkipStats)> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let keep = |rel: &Path| {
        selected
            .map(|s| s.iter().any(|x| x.as_path() == rel))
            .unwrap_or(true)
    };
    let (locals, mut skipped) = list_local_rels(config_dir, app)?;
    for f in locals {
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
        if out.len() >= MAX_FILES_PER_APP {
            skipped.truncated = true;
            break;
        }
        out.push(FileCompare {
            bytes: repo_file.symlink_metadata().map(|m| m.len()).unwrap_or(0),
            rel,
            state: FileState::MissingLocal,
        });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok((out, skipped))
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
/// Plans without changes are a no-op: no backup litter, nothing rewritten.
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
    if plan.iter().all(|o| o.kind == OpKind::Unchanged) {
        report.unchanged = plan.len();
        return Ok(report);
    }
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
    // No-op fast path: nothing selected that would change — leave the live
    // tree (and the backup list) untouched instead of rename+copy churn.
    let actionable = plan
        .iter()
        .any(|o| o.kind != OpKind::Unchanged && selected.contains(&o.rel));
    if !actionable {
        report.unchanged = plan.iter().filter(|o| selected.contains(&o.rel)).count();
        return Ok(report);
    }
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
            // Always a FRESH backup: bump the suffix until the name is free,
            // so a second apply within the same second can never silently
            // skip the backup (and merge back stale content afterwards).
            let mut n = 0u32;
            let mut bak = PathBuf::from(format!("{}.bak.{epoch}", target.display()));
            while bak.exists() {
                n += 1;
                bak = PathBuf::from(format!("{}.bak.{epoch}-{n}", target.display()));
            }
            std::fs::rename(&target, &bak).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("backup failed for {}: {e}", target.display()),
                )
            })?;
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
    // Generic single-root layout: every app owns exactly one top-level
    // entry, repo paths mirror beneath it. No per-app mapping tables.
    config_dir.join(app.root()).join(rel)
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
    if ma.len() == 0 {
        return Ok(false);
    }
    if !ma.is_symlink() && ma.len() > MAX_FILE_BYTES {
        // Giants compare by size only (see `compare`): same size counts as
        // synced so every refresh doesn't re-read gigabytes.
        return Ok(false);
    }
    // Chunked compare: bounded memory + early exit on the first
    // differing block (the old code read both files fully every time).
    const CHUNK: usize = 64 * 1024;
    let mut fa = std::fs::File::open(a)?;
    let mut fb = std::fs::File::open(b)?;
    let mut ba = vec![0u8; CHUNK];
    let mut bb = vec![0u8; CHUNK];
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(true);
        }
        if na == 0 {
            return Ok(false);
        }
    }
}

/// Fill `buf`, short only at EOF. Returns bytes read.
fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> io::Result<usize> {
    use std::io::Read;
    let mut got = 0;
    while got < buf.len() {
        match f.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Ok(got)
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
    if is_backup_name(path) {
        // Our own recovery litter (`*.bak.<epoch>`): created by Apply on this
        // machine, must never be synced back into the repo and re-pushed.
        return true;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| exclude.iter().any(|e| e == n))
        .unwrap_or(false)
}

/// Backup file name (`settings.json.bak.175…`)? Matches restores created by
/// Apply/Restore, never user content (which has no `.bak.` infix).
fn is_backup_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.contains(".bak."))
        .unwrap_or(false)
}

fn is_excluded_rel(rel: &Path, exclude: &[String]) -> bool {
    if is_backup_name(rel) {
        return true;
    }
    rel.components().any(|c| {
        c.as_os_str()
            .to_str()
            .map(|n| exclude.iter().any(|e| e == n))
            .unwrap_or(false)
    })
}

fn write_manifest(repo_app_dir: &Path, app_id: &str, files: usize) -> io::Result<()> {
    std::fs::create_dir_all(repo_app_dir)?;
    // Idempotent: a snapshot that changed nothing but the timestamp must not
    // dirty the repo — otherwise every Push commits a timestamp-only "sync"
    // forever and never reports "nothing to commit".
    if manifest_covers(repo_app_dir.join("manifest.json"), app_id, files) {
        return Ok(());
    }
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

/// True when the existing manifest already records this app + file count
/// (only the timestamp would change — not worth dirtying the repo for).
fn manifest_covers(manifest: PathBuf, app_id: &str, files: usize) -> bool {
    let Ok(old) = std::fs::read_to_string(&manifest) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&old) else {
        return false;
    };
    v.get("app").and_then(|a| a.as_str()) == Some(app_id)
        && v.get("files").and_then(|f| f.as_u64()) == Some(files as u64)
        && v.get("schema_version").and_then(|s| s.as_u64()) == Some(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::resolve_app;

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
        let app = resolve_app(Path::new(""), "hypr");
        let repo = tmp.path().join("repo/hypr");
        let r = snapshot_to_repo(&cfg, &repo, &app).unwrap();
        assert_eq!(r.copied, 1);
        assert_eq!(r.skipped, SkipStats::default());
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
        let app = resolve_app(Path::new(""), "zed");
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
        let app = resolve_app(Path::new(""), "zed");
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
        let app = resolve_app(Path::new(""), "zed");
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
        let app = resolve_app(Path::new(""), "zed");
        let r = snapshot_to_repo(&cfg, &repo, &app).unwrap();
        assert_eq!(r.copied, 2);
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
        let app = resolve_app(Path::new(""), "zed");
        let (cmp, skipped) = compare(&cfg, &repo, &app, None).unwrap();
        assert_eq!(skipped, SkipStats::default());
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
        let app = resolve_app(Path::new(""), "zed");
        // push only a.json
        let sel: HashSet<PathBuf> = [PathBuf::from("a.json")].into();
        assert_eq!(
            snapshot_selected(&cfg, &repo, &app, &sel).unwrap().copied,
            1
        );
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
        let app_hypr = resolve_app(Path::new(""), "hypr");
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

    #[test]
    fn noop_apply_creates_no_backup_litter() {
        use std::collections::HashSet;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let repo = tmp.path().join("repo/zed");
        write(&cfg.join("zed/settings.json"), "same");
        write(&repo.join("settings.json"), "same");
        let app = resolve_app(Path::new(""), "zed");
        let plan = preview(&cfg, &repo, &app).unwrap();
        assert!(plan.iter().all(|o| o.kind == OpKind::Unchanged));
        let r = apply_plan(&cfg, &repo, &app, &plan).unwrap();
        assert_eq!(r.written, 0);
        assert_eq!(r.unchanged, plan.len());
        assert!(r.backed_up.is_empty());
        assert!(list_backups(&cfg, &app).is_empty());
        // selective variant behaves the same
        let sel: HashSet<PathBuf> = [PathBuf::from("settings.json")].into();
        let r2 = apply_plan_selected(&cfg, &repo, &app, &plan, &sel).unwrap();
        assert_eq!(r2.written, 0);
        assert_eq!(r2.merged_back, 0);
        assert!(r2.backed_up.is_empty());
        assert!(list_backups(&cfg, &app).is_empty());
    }

    #[test]
    fn chunked_diff_finds_late_difference() {
        // 200 KB identical prefix, last byte differs.
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.bin");
        let b = tmp.path().join("b.bin");
        let mut v = vec![0xABu8; 200 * 1024];
        std::fs::write(&a, &v).unwrap();
        v[200 * 1024 - 1] = 0xCD;
        std::fs::write(&b, &v).unwrap();
        assert!(super::files_differ(&a, &b).unwrap());
        std::fs::write(&b, vec![0xABu8; 200 * 1024]).unwrap();
        assert!(!super::files_differ(&a, &b).unwrap());
        // empty files are equal, missing file errors (caller maps to Modified)
        let e = tmp.path().join("e");
        std::fs::write(&e, b"").unwrap();
        let e2 = tmp.path().join("e2");
        std::fs::write(&e2, b"").unwrap();
        assert!(!super::files_differ(&e, &e2).unwrap());
        assert!(super::files_differ(&e, &tmp.path().join("nope")).is_err());
    }

    #[test]
    fn large_files_skip_snapshot_and_list_but_compare_by_size() {
        use std::collections::HashSet;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let repo = tmp.path().join("repo/big");
        // Sparse 26 MB file: instant to create, reads as zeros.
        let big = cfg.join("big/blob.bin");
        std::fs::create_dir_all(big.parent().unwrap()).unwrap();
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_FILE_BYTES + 1024).unwrap();
        write(&cfg.join("big/small.json"), "{}");
        let app = AppSpec {
            id: "big".to_string(),
            label: "Big".to_string(),
            rel_paths: vec!["big".to_string()],
            exclude_files: vec![],
        };
        let (locals, skipped) = list_local_rels(&cfg, &app).unwrap();
        assert_eq!(locals.len(), 1); // only small.json
        assert_eq!(skipped.large, 1);
        let sel: HashSet<PathBuf> = [PathBuf::from("small.json")].into();
        let r = snapshot_selected(&cfg, &repo, &app, &sel).unwrap();
        // small.json copied; the giant is counted even though unselected —
        // the walk saw it and deliberately left it out.
        assert_eq!((r.copied, r.skipped.large), (1, 1));
        // Equal-size giants compare by size only (no 26 MB read).
        let staged = repo.join("blob.bin");
        std::fs::create_dir_all(repo.clone()).unwrap();
        let g = std::fs::File::create(&staged).unwrap();
        g.set_len(MAX_FILE_BYTES + 1024).unwrap();
        assert!(!super::files_differ(&big, &staged).unwrap());
        let h = std::fs::File::create(tmp.path().join("short.bin")).unwrap();
        h.set_len(8).unwrap();
        assert!(super::files_differ(&big, tmp.path().join("short.bin").as_path()).unwrap());
    }

    #[test]
    fn own_backups_never_sync() {
        use std::collections::HashSet;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let repo = tmp.path().join("repo/zed");
        write(&cfg.join("zed/settings.json"), "{}");
        write(&cfg.join("zed/settings.json.bak.123"), "old");
        let app = resolve_app(Path::new(""), "zed");
        let (locals, skipped) = list_local_rels(&cfg, &app).unwrap();
        assert_eq!(locals.len(), 1);
        assert_eq!(skipped, SkipStats::default());
        let sel: HashSet<PathBuf> = [
            PathBuf::from("settings.json"),
            PathBuf::from("settings.json.bak.123"),
        ]
        .into();
        let r = snapshot_selected(&cfg, &repo, &app, &sel).unwrap();
        assert_eq!(r.copied, 1);
        assert!(!repo.join("settings.json.bak.123").exists());
        // …and repo-side litter stays invisible to compare.
        write(&repo.join("stale.bak.9"), "x");
        let (cmp, _) = compare(&cfg, &repo, &app, None).unwrap();
        assert!(cmp
            .iter()
            .all(|c| !c.rel.to_string_lossy().contains(".bak.")));
    }

    #[test]
    fn walk_stops_at_file_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let dir = cfg.join("many");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..(MAX_FILES_PER_APP + 5) {
            std::fs::write(dir.join(format!("f{i:05}.json")), "{}").unwrap();
        }
        let app = AppSpec {
            id: "many".to_string(),
            label: "Many".to_string(),
            rel_paths: vec!["many".to_string()],
            exclude_files: vec![],
        };
        let (locals, skipped) = list_local_rels(&cfg, &app).unwrap();
        assert_eq!(locals.len(), MAX_FILES_PER_APP);
        assert!(skipped.truncated);
    }

    #[test]
    fn manifest_rewrite_is_idempotent() {
        use std::collections::HashSet;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config");
        let repo = tmp.path().join("repo/zed");
        write(&cfg.join("zed/settings.json"), "{}");
        let app = resolve_app(Path::new(""), "zed");
        let sel: HashSet<PathBuf> = [PathBuf::from("settings.json")].into();
        snapshot_selected(&cfg, &repo, &app, &sel).unwrap();
        let first = std::fs::read(repo.join("manifest.json")).unwrap();
        // A snapshot that changes nothing must leave the manifest
        // byte-identical (the epoch alone must not dirty the repo, or every
        // Push commits a timestamp-only "sync" forever). The sleep rules out
        // a same-second false pass.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        snapshot_selected(&cfg, &repo, &app, &sel).unwrap();
        assert_eq!(std::fs::read(repo.join("manifest.json")).unwrap(), first);
        // …while a real change (new file count) still refreshes it.
        write(&cfg.join("zed/extra.json"), "{}");
        let sel2: HashSet<PathBuf> =
            [PathBuf::from("settings.json"), PathBuf::from("extra.json")].into();
        snapshot_selected(&cfg, &repo, &app, &sel2).unwrap();
        assert_ne!(std::fs::read(repo.join("manifest.json")).unwrap(), first);
    }
}
