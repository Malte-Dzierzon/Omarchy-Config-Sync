//! App catalog + live discovery.
//!
//! - Curated presets carry excludes (machine-local files) and multi-path
//!   mappings (e.g. omarchy shell).
//! - [`discover_apps`] merges presets with everything actually present in the
//!   config dir, so other people's apps (and hand-added ones) show up too.
//!   Favorites stay pinned on top, the rest is alphabetical.
//! - Owned [`AppSpec`] (no 'static strings) so discovered apps work everywhere.

use std::path::Path;

/// Pinned favorites, in order: zed, hyprland, omarchy shell, kitty,
/// fastfetch, neovim.
pub const FAVORITES: [&str; 6] = ["zed", "hypr", "omarchy-shell", "kitty", "fastfetch", "nvim"];

#[derive(Debug, Clone)]
pub struct AppSpec {
    pub id: String,
    pub label: String,
    /// Files/dirs relative to config dir, e.g. `zed` or `omarchy/shell.json`.
    pub rel_paths: Vec<String>,
    /// Basenames never synced (machine-local), e.g. `monitors.lua`.
    pub exclude_files: Vec<String>,
}

impl AppSpec {
    pub fn is_favorite(&self) -> bool {
        FAVORITES.contains(&self.id.as_str())
    }
}

fn preset(id: &str, label: &str, rel_paths: &[&str], exclude_files: &[&str]) -> AppSpec {
    AppSpec {
        id: id.to_string(),
        label: label.to_string(),
        rel_paths: rel_paths.iter().map(|s| s.to_string()).collect(),
        exclude_files: exclude_files.iter().map(|s| s.to_string()).collect(),
    }
}

pub fn builtin_apps() -> Vec<AppSpec> {
    vec![
        preset("zed", "Zed", &["zed"], &[]),
        preset(
            "hypr",
            "Hyprland",
            &["hypr"],
            &["monitors.lua", "input.lua"],
        ),
        preset(
            "omarchy-shell",
            "Omarchy shell",
            &["omarchy/shell.json", "omarchy/extensions"],
            &[],
        ),
        preset("alacritty", "Alacritty", &["alacritty"], &[]),
        preset("ghostty", "Ghostty", &["ghostty"], &[]),
        preset("foot", "Foot", &["foot"], &[]),
        preset("kitty", "Kitty", &["kitty"], &[]),
        preset("starship", "Starship", &["starship.toml"], &[]),
        preset("btop", "btop", &["btop"], &[]),
        preset("lazygit", "Lazygit", &["lazygit"], &[]),
        preset("fastfetch", "Fastfetch", &["fastfetch"], &[]),
        preset("nvim", "Neovim", &["nvim"], &[]),
    ]
}

/// Curated preset by id.
pub fn find_app(id: &str) -> Option<AppSpec> {
    builtin_apps().into_iter().find(|a| a.id == id)
}

/// Any id (preset or present-on-disk) resolves to a spec. Unknown ids become
/// a plain single-root spec, so cloned repos and hand-added configs just work.
pub fn resolve_app(_config_dir: &Path, id: &str) -> AppSpec {
    if let Some(p) = find_app(id) {
        return p;
    }
    AppSpec {
        id: id.to_string(),
        label: prettify(id),
        rel_paths: vec![id.to_string()],
        exclude_files: Vec::new(),
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

/// Favorites pinned first (in [`FAVORITES`] order), then every other preset
/// plus every discovered config-dir entry, alphabetical by label.
/// Preset-claimed top-level names (e.g. `omarchy`) are not duplicated.
pub fn discover_apps(config_dir: &Path) -> Vec<AppSpec> {
    let presets = builtin_apps();
    let claimed: Vec<String> = presets
        .iter()
        .flat_map(|a| a.rel_paths.iter())
        .map(|r| r.split('/').next().unwrap_or(r).to_string())
        .collect();
    let mut out: Vec<AppSpec> = FAVORITES
        .iter()
        .filter_map(|fid| presets.iter().find(|a| &a.id == fid).cloned())
        .collect();
    let mut rest: Vec<AppSpec> = presets
        .into_iter()
        .filter(|a| !FAVORITES.contains(&a.id.as_str()))
        .collect();
    if let Ok(rd) = std::fs::read_dir(config_dir) {
        let mut names: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for n in names {
            if claimed.iter().any(|c| c == &n) {
                continue;
            }
            rest.push(AppSpec {
                id: n.clone(),
                label: prettify(&n),
                rel_paths: vec![n],
                exclude_files: Vec::new(),
            });
        }
    }
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
        assert!(ids.contains(&"kitty")); // preset even when missing
        assert!(resolve_app(cfg, "mycoolapp").rel_paths == vec!["mycoolapp".to_string()]);
    }
}
