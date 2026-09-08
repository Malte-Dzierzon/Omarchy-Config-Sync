//! App catalog: fully generic, zero per-app definitions.
//!
//! Every top-level entry in the config dir is an "app" owning exactly that
//! one entry (`rel_paths == [id]`). Present-on-disk is the only registry —
//! cloned repos and hand-added configs work with zero registration, on any
//! machine. The only curated lists left:
//! - [`FAVORITES`]: pinned sidebar order.
//! - [`MACHINE_LOCAL`]: basenames that stay local on every app (per-machine
//!   files like monitor layouts must not hop between PCs).

use std::path::Path;

/// Pinned favorites, in order: zed, hyprland, omarchy shell, kitty,
/// fastfetch, neovim. Always listed (missing ones show "not installed").
pub const FAVORITES: [&str; 6] = ["zed", "hypr", "omarchy-shell", "kitty", "fastfetch", "nvim"];

/// Basenames never synced, on every app. Applied globally so no per-app
/// preset table is needed to protect machine-local files.
pub const MACHINE_LOCAL: &[&str] = &["monitors.lua", "input.lua"];

#[derive(Debug, Clone)]
pub struct AppSpec {
    pub id: String,
    pub label: String,
    /// Exactly one top-level entry (see [`AppSpec::root`]).
    pub rel_paths: Vec<String>,
    /// Basenames never synced (machine-local), from [`MACHINE_LOCAL`].
    pub exclude_files: Vec<String>,
}

impl AppSpec {
    pub fn is_favorite(&self) -> bool {
        FAVORITES.contains(&self.id.as_str())
    }

    /// The single top-level entry this app owns.
    pub fn root(&self) -> &str {
        self.rel_paths
            .first()
            .map(String::as_str)
            .unwrap_or(&self.id)
    }
}

/// Any id resolves to a spec — no registry lookup, no missing case.
pub fn resolve_app(_config_dir: &Path, id: &str) -> AppSpec {
    AppSpec {
        id: id.to_string(),
        label: prettify(id),
        rel_paths: vec![id.to_string()],
        exclude_files: MACHINE_LOCAL.iter().map(|s| s.to_string()).collect(),
    }
}

fn prettify(id: &str) -> String {
    let spaced = id.replace(['-', '_', '.'], " ");
    let mut c = spaced.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => id.to_string(),
    }
}

/// Favorites pinned first (in [`FAVORITES`] order), then every top-level
/// config-dir entry, alphabetical by label.
pub fn discover_apps(config_dir: &Path) -> Vec<AppSpec> {
    let mut out: Vec<AppSpec> = FAVORITES
        .iter()
        .map(|fid| resolve_app(config_dir, fid))
        .collect();
    let mut names: Vec<String> = match std::fs::read_dir(config_dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    let mut rest: Vec<AppSpec> = names
        .into_iter()
        .filter(|n| !FAVORITES.contains(&n.as_str()))
        .map(|n| resolve_app(config_dir, &n))
        .collect();
    rest.sort_by(|a, b| a.label.cmp(&b.label));
    out.extend(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_pins_favorites_and_finds_custom() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path();
        std::fs::create_dir_all(cfg.join("zed")).unwrap();
        std::fs::create_dir_all(cfg.join("mycoolapp")).unwrap();
        let apps = discover_apps(cfg);
        for (i, f) in FAVORITES.iter().enumerate() {
            assert_eq!(apps[i].id, *f);
        }
        let ids: Vec<_> = apps.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"mycoolapp"));
        // favorites are listed even when missing from disk
        assert!(ids.contains(&"kitty"));
        let custom = resolve_app(cfg, "mycoolapp");
        assert_eq!(custom.rel_paths, vec!["mycoolapp".to_string()]);
        assert_eq!(custom.root(), "mycoolapp");
    }

    #[test]
    fn machine_local_excludes_apply_to_every_app() {
        let tmp = tempfile::tempdir().unwrap();
        let a = resolve_app(tmp.path(), "anything-at-all");
        assert_eq!(a.exclude_files, vec!["monitors.lua", "input.lua"]);
    }
}
