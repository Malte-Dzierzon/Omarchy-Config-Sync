//! Tiny persistent prefs, zero deps. Currently a single flag: whether the
//! welcome screen stays hidden (`never_show_welcome`, the "Don't show again"
//! checkbox). Stored as one `key=value` line under `$XDG_STATE_HOME` (else
//! `~/.local/state`), so deleting the file restores the welcome screen.

use std::io;
use std::path::{Path, PathBuf};

const KEY: &str = "never_show_welcome";

/// Testable path builder: `$XDG/omarchy-config-sync/prefs`.
pub fn path_for(home: Option<&Path>, xdg: Option<&Path>) -> PathBuf {
    let base = xdg
        .filter(|p| p.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"));
    base.join("omarchy-config-sync").join("prefs")
}

/// Env-based [`path_for`] for the live system.
pub fn prefs_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    path_for(home.as_deref(), xdg.as_deref())
}

/// Read the flag; missing/unreadable/garbled file means "show welcome".
pub fn load_never_show() -> bool {
    load_from(&prefs_path())
}

/// Testable [`load_never_show`].
pub fn load_from(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|l| {
        let l = l.trim();
        l == format!("{KEY}=1") || l == format!("{KEY}=true")
    })
}

/// Persist the flag (creates parent dirs). Deleting the file re-enables welcome.
pub fn save_never_show(v: bool) -> io::Result<()> {
    save_to(&prefs_path(), v)
}

/// Testable [`save_never_show`].
pub fn save_to(path: &Path, v: bool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{KEY}={}\n", if v { 1 } else { 0 }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_means_show() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!load_from(&tmp.path().join("prefs")));
    }

    #[test]
    fn roundtrip_and_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("sub").join("prefs");
        save_to(&p, true).unwrap();
        assert!(load_from(&p));
        save_to(&p, false).unwrap();
        assert!(!load_from(&p));
        std::fs::write(&p, "junk\n").unwrap();
        assert!(!load_from(&p));
    }

    #[test]
    fn path_prefers_xdg_then_home() {
        let out = path_for(Some(Path::new("/home/u")), Some(Path::new("/home/u/.st")));
        assert_eq!(out, PathBuf::from("/home/u/.st/omarchy-config-sync/prefs"));
        let out = path_for(Some(Path::new("/home/u")), None);
        assert_eq!(
            out,
            PathBuf::from("/home/u/.local/state/omarchy-config-sync/prefs")
        );
    }
}
