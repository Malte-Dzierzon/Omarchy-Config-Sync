//! Live Omarchy theme reader (colors + corner style). Read-only, zero extra deps.
//!
//! Sources (all files, never commands):
//! - Theme slug: `<state>/omarchy/current/theme.name` (e.g. `haven`)
//! - Theme dir: `<config>/omarchy/themes/<slug>/` (user) else
//!   `<omarchy>/themes/<slug>/` (stock), case-insensitive fallback
//! - Colors: flat `colors.toml` (`accent = "#97a6bb"`), hand-parsed
//! - Radius: `rounding = N` from the theme's Hyprland files, else
//!   `<config>/hypr/looknfeel.lua`, else 10. (Omarchy themes ship no radius
//!   value of their own; Hyprland rounding is what shapes the desktop.)
//!
//! Everything falls back to muted-dark defaults, so the GUI also runs
//! on non-Omarchy machines or with a half-missing theme.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub fn from_hex(s: &str) -> Option<Rgb> {
        let h = s.trim().trim_start_matches('#');
        if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        Some(Rgb {
            r: (v >> 16) as u8,
            g: (v >> 8) as u8,
            b: v as u8,
        })
    }
}

#[derive(Debug, Clone)]
pub struct OmarchyTheme {
    pub slug: String,
    pub display_name: String,
    pub dark: bool,
    pub background: Rgb,
    pub surface: Rgb,
    pub surface_dark: Rgb,
    pub foreground: Rgb,
    pub muted: Rgb,
    pub selection: Rgb,
    pub accent: Rgb,
    pub success: Rgb,
    pub warning: Rgb,
    pub danger: Rgb,
    pub radius: f32,
}

impl Default for OmarchyTheme {
    /// Muted-dark fallback (Haven-ish) for non-Omarchy machines.
    fn default() -> Self {
        OmarchyTheme {
            slug: String::new(),
            display_name: "Default".to_string(),
            dark: true,
            background: Rgb {
                r: 0x07,
                g: 0x0e,
                b: 0x15,
            },
            surface: Rgb {
                r: 0x20,
                g: 0x26,
                b: 0x2c,
            },
            surface_dark: Rgb {
                r: 0x05,
                g: 0x0b,
                b: 0x10,
            },
            foreground: Rgb {
                r: 0xe9,
                g: 0xef,
                b: 0xeb,
            },
            muted: Rgb {
                r: 0x62,
                g: 0x67,
                b: 0x6c,
            },
            selection: Rgb {
                r: 0x2b,
                g: 0x33,
                b: 0x3a,
            },
            accent: Rgb {
                r: 0x97,
                g: 0xa6,
                b: 0xbb,
            },
            success: Rgb {
                r: 0x82,
                g: 0x96,
                b: 0x7f,
            },
            warning: Rgb {
                r: 0x96,
                g: 0x93,
                b: 0x7b,
            },
            danger: Rgb {
                r: 0x85,
                g: 0x79,
                b: 0x60,
            },
            radius: 10.0,
        }
    }
}

/// Env-based entry point: `$XDG_CONFIG_HOME`/`~/.config`,
/// `$XDG_STATE_HOME`/`~/.local/state`, `$OMARCHY_PATH`.
pub fn load() -> OmarchyTheme {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config_dir = crate::scanner::resolve_config_dir();
    let state_dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.clone().map(|h| h.join(".local/state")))
        .map(|s| s.join("omarchy"))
        .unwrap_or_else(|| PathBuf::from(".local/state/omarchy"));
    let stock_themes = std::env::var_os("OMARCHY_PATH")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/usr/share/omarchy"))
        .join("themes");
    load_from(&config_dir, &state_dir, &stock_themes)
}

/// Testable entry point with explicit directories.
pub fn load_from(config_dir: &Path, state_dir: &Path, stock_themes: &Path) -> OmarchyTheme {
    let mut t = OmarchyTheme::default();
    if let Ok(raw) = std::fs::read_to_string(state_dir.join("current/theme.name")) {
        let slug = raw.trim().to_lowercase().replace([' ', '_'], "-");
        if !slug.is_empty() && slug != "unknown" {
            t.display_name = display_name(&slug);
            t.slug = slug;
        }
    }
    let user_themes = config_dir.join("omarchy/themes");
    if let Some(dir) = theme_dir(&user_themes, stock_themes, &t.slug) {
        if let Ok(text) = std::fs::read_to_string(dir.join("colors.toml")) {
            apply_colors(&mut t, &text);
        }
        if let Some(r) = find_rounding_in_dir(&dir) {
            t.radius = r;
        }
    }
    // Corner style follows the desktop when the theme carries none.
    if t.radius == OmarchyTheme::default().radius {
        if let Ok(text) = std::fs::read_to_string(config_dir.join("hypr/looknfeel.lua")) {
            if let Some(r) = find_rounding(&text) {
                t.radius = r;
            }
        }
    }
    t
}

/// Cheap change fingerprint for live reload: slug + mtimes of the files that
/// feed the theme. The GUI polls this and reapplies on change.
pub fn fingerprint(config_dir: &Path, state_dir: &Path, stock_themes: &Path) -> String {
    let slug = std::fs::read_to_string(state_dir.join("current/theme.name"))
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default();
    let user_themes = config_dir.join("omarchy/themes");
    let dir = theme_dir(&user_themes, stock_themes, &slug);
    let colors_mtime = dir
        .as_ref()
        .map(|d| mtime_str(&d.join("colors.toml")))
        .unwrap_or_default();
    format!(
        "{slug}|{colors_mtime}|{}",
        mtime_str(&config_dir.join("hypr/looknfeel.lua"))
    )
}

/// Env-based [`fingerprint`] for the live system.
pub fn fingerprint_live() -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config_dir = crate::scanner::resolve_config_dir();
    let state_dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.clone().map(|h| h.join(".local/state")))
        .map(|s| s.join("omarchy"))
        .unwrap_or_else(|| PathBuf::from(".local/state/omarchy"));
    let stock_themes = std::env::var_os("OMARCHY_PATH")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/usr/share/omarchy"))
        .join("themes");
    fingerprint(&config_dir, &state_dir, &stock_themes)
}

fn mtime_str(p: &Path) -> String {
    std::fs::symlink_metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

/// `catppuccin-latte` -> `Catppuccin Latte` (mirrors `omarchy theme current`).
pub fn display_name(slug: &str) -> String {
    slug.split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn theme_dir(user_themes: &Path, stock_themes: &Path, slug: &str) -> Option<PathBuf> {
    if slug.is_empty() {
        return None;
    }
    let exact_user = user_themes.join(slug);
    if exact_user.join("colors.toml").is_file() {
        return Some(exact_user);
    }
    let exact_stock = stock_themes.join(slug);
    if exact_stock.join("colors.toml").is_file() {
        return Some(exact_stock);
    }
    // User dirs vary in case ("Natur", "a-fogt"): scan both roots once.
    for root in [user_themes, stock_themes] {
        let Ok(rd) = std::fs::read_dir(root) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir()
                && p.file_name().and_then(|n| n.to_str()) == Some(slug)
                && p.join("colors.toml").is_file()
            {
                return Some(p);
            }
            if p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_lowercase() == slug)
                .unwrap_or(false)
                && p.join("colors.toml").is_file()
            {
                return Some(p);
            }
        }
    }
    None
}

fn apply_colors(t: &mut OmarchyTheme, text: &str) {
    for (k, v) in parse_flat(text) {
        match k.as_str() {
            "mode" => t.dark = v != "light",
            "background" => set(&mut t.background, &v),
            "lighter_background" => set(&mut t.surface, &v),
            "dark_background" | "darker_background" => set(&mut t.surface_dark, &v),
            "foreground" | "light_foreground" | "bright_foreground" => set(&mut t.foreground, &v),
            "muted" | "dark_foreground" => set(&mut t.muted, &v),
            "selection" => set(&mut t.selection, &v),
            "accent" | "blue" => set(&mut t.accent, &v),
            "green" => set(&mut t.success, &v),
            "yellow" => set(&mut t.warning, &v),
            "red" => set(&mut t.danger, &v),
            _ => {}
        }
    }
}

fn set(slot: &mut Rgb, v: &str) {
    if let Some(c) = Rgb::from_hex(v) {
        *slot = c;
    }
}

/// Minimal flat `key = "value"` parser (colors.toml has no tables).
/// Full-line `#`/`[` lines are skipped; quoted values keep their `#`.
fn parse_flat(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some(eq) = line.find('=') else {
            continue;
        };
        let (k, v) = (line[..eq].trim(), line[eq + 1..].trim());
        if k.is_empty() {
            continue;
        }
        let val = if let Some(rest) = v.strip_prefix('"') {
            rest.split('"').next().unwrap_or("").to_string()
        } else if let Some(rest) = v.strip_prefix('\'') {
            rest.split('\'').next().unwrap_or("").to_string()
        } else {
            v.split('#').next().unwrap_or("").trim().to_string()
        };
        out.push((k.to_string(), val));
    }
    out
}

/// First `rounding = N` identifier in Hyprland lua/conf text (0..=32).
/// Skips lookalikes like `gradient_rounding`.
pub fn find_rounding(text: &str) -> Option<f32> {
    for line in text.lines() {
        let b = line.as_bytes();
        let mut i = 0;
        while i + 8 <= b.len() {
            if &b[i..i + 8] == b"rounding"
                && (i == 0 || !is_ident(b[i - 1]))
                && (i + 8 >= b.len() || !is_ident(b[i + 8]))
            {
                let rest = &line[i + 8..];
                if let Some(eq) = rest.find('=') {
                    let num: String = rest[eq + 1..]
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if let Ok(n) = num.parse::<f32>() {
                        return Some(n.clamp(0.0, 32.0));
                    }
                }
            }
            i += 1;
        }
    }
    None
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Search a theme dir (depth <= 2, lua/conf only) for a rounding value.
fn find_rounding_in_dir(dir: &Path) -> Option<f32> {
    let mut stack = vec![(dir.to_path_buf(), 0u8)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if depth < 2 {
                    stack.push((p, depth + 1));
                }
            } else if p.extension().and_then(|x| x.to_str()) == Some("lua")
                || p.extension().and_then(|x| x.to_str()) == Some("conf")
            {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    if let Some(r) = find_rounding(&text) {
                        return Some(r);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAVEN_LIKE: &str = "# comment\nmode = \"dark\"\n\naccent = \"#97a6bb\"\nbackground = \"#070e15\"\nplain = nope # trailing\n[ignored]\nx = 1\n";

    #[test]
    fn parses_flat_colors_and_skips_noise() {
        let t = {
            let mut th = OmarchyTheme::default();
            apply_colors(&mut th, HAVEN_LIKE);
            th
        };
        assert!(t.dark);
        assert_eq!(
            t.accent,
            Rgb {
                r: 0x97,
                g: 0xa6,
                b: 0xbb
            }
        );
        assert_eq!(
            t.background,
            Rgb {
                r: 0x07,
                g: 0x0e,
                b: 0x15
            }
        );
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(Rgb::from_hex("red").is_none());
        assert!(Rgb::from_hex("#12345").is_none());
        assert!(Rgb::from_hex("#gggggg").is_none());
        assert!(Rgb::from_hex("#070e15").is_some());
    }

    #[test]
    fn rounding_hits_identifier_only_and_clamps() {
        assert_eq!(find_rounding("rounding = 12,"), Some(12.0));
        assert_eq!(find_rounding("  rounding=0"), Some(0.0));
        assert_eq!(find_rounding("gradient_rounding = 9"), None);
        assert_eq!(find_rounding("nothing here"), None);
        assert_eq!(find_rounding("rounding = 99"), Some(32.0));
    }

    #[test]
    fn display_name_title_cases() {
        assert_eq!(display_name("haven"), "Haven");
        assert_eq!(display_name("catppuccin-latte"), "Catppuccin Latte");
    }

    #[test]
    fn missing_everything_yields_default() {
        let tmp = tempfile::tempdir().unwrap();
        let t = load_from(
            &tmp.path().join("cfg"),
            &tmp.path().join("state"),
            &tmp.path().join("stock"),
        );
        assert_eq!(t.radius, 10.0);
        assert!(t.dark);
    }

    #[test]
    fn user_theme_wins_and_radius_flows() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("cfg");
        let state = tmp.path().join("state");
        let stock = tmp.path().join("stock");
        std::fs::create_dir_all(state.join("current")).unwrap();
        std::fs::write(state.join("current/theme.name"), "mytheme\n").unwrap();
        std::fs::create_dir_all(cfg.join("omarchy/themes/mytheme")).unwrap();
        std::fs::write(
            cfg.join("omarchy/themes/mytheme/colors.toml"),
            "mode = \"light\"\naccent = \"#112233\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(cfg.join("hypr")).unwrap();
        std::fs::write(cfg.join("hypr/looknfeel.lua"), "rounding = 7,").unwrap();
        let t = load_from(&cfg, &state, &stock);
        assert_eq!(t.display_name, "Mytheme");
        assert!(!t.dark);
        assert_eq!(
            t.accent,
            Rgb {
                r: 0x11,
                g: 0x22,
                b: 0x33
            }
        );
        assert_eq!(t.radius, 7.0);
    }

    #[test]
    fn selection_parses_and_fingerprint_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("cfg");
        let state = tmp.path().join("state");
        let stock = tmp.path().join("stock");
        std::fs::create_dir_all(state.join("current")).unwrap();
        std::fs::write(state.join("current/theme.name"), "t\n").unwrap();
        std::fs::create_dir_all(cfg.join("omarchy/themes/t")).unwrap();
        std::fs::write(
            cfg.join("omarchy/themes/t/colors.toml"),
            "selection = \"#aabbcc\"\n",
        )
        .unwrap();
        let t = load_from(&cfg, &state, &stock);
        assert_eq!(
            t.selection,
            Rgb {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc
            }
        );
        let fp1 = fingerprint(&cfg, &state, &stock);
        std::fs::write(state.join("current/theme.name"), "other\n").unwrap();
        let fp2 = fingerprint(&cfg, &state, &stock);
        assert_ne!(fp1, fp2);
    }
}
