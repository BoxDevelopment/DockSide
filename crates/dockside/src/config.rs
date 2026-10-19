use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use serde::Deserialize;

/// Single source of truth for defaults: the user's file is deep-merged over this.
pub const DEFAULT_TOML: &str = include_str!("../default_config.toml");

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Config {
    pub general: General,
    pub menubar: MenuBarCfg,
    pub dock: DockCfg,
    pub spotlight: SpotlightCfg,
    pub finder: FinderCfg,
    pub island: IslandCfg,
    #[serde(default)]
    pub app_names: BTreeMap<String, String>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct General {
    pub appearance: Appearance,
    pub font: String,
    pub accent: String,
    pub hide_taskbar: bool,
    pub shortcut_style: ShortcutStyle,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    Auto,
    Light,
    Dark,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Backdrop {
    Acrylic,
    Blur,
    None,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IconStyle {
    Native,
    Squircle,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ShortcutStyle {
    Mac,
    Windows,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct MenuBarCfg {
    pub enabled: bool,
    pub height: f32,
    pub backdrop: Backdrop,
    pub tint_light: String,
    pub tint_dark: String,
    pub logo: String,
    pub clock_format: String,
    pub show_battery: bool,
    pub show_battery_percent: bool,
    pub show_wifi: bool,
    pub show_search: bool,
    pub show_control_center: bool,
    pub desktop_app_name: String,
    pub reserve_space: bool,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct DockCfg {
    pub enabled: bool,
    pub tile_size: f32,
    pub magnification: bool,
    pub magnified_size: f32,
    pub bottom_margin: f32,
    pub backdrop: Backdrop,
    pub tint_light: String,
    pub tint_dark: String,
    pub icon_style: IconStyle,
    pub show_running: bool,
    pub show_trash: bool,
    pub autohide: bool,
    pub reserve_space: bool,
    #[serde(default)]
    pub apps: Vec<DockApp>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct SpotlightCfg {
    pub enabled: bool,
    pub hotkey: String,
    pub search_engine: String,
    pub custom_search_url: String,
}

/// Finder window defaults. View, sort and hidden files are saved whenever they change in a Finder window.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct FinderCfg {
    pub view: crate::finder::View,
    pub sort: crate::finder::fs::SortKey,
    pub ascending: bool,
    pub show_hidden: bool,
    pub sidebar_vibrancy: bool,
}

/// The Dynamic Island at the top center of the screen.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct IslandCfg {
    pub enabled: bool,
    /// Keep the idle pill visible when nothing is happening.
    pub always_show: bool,
    pub expand_on_hover: bool,
    /// Seconds paused media stays in the island (0 = until the app ends its session).
    pub hide_paused_after: u32,
    pub battery_alerts: bool,
    /// Replace the Windows volume / brightness popups.
    pub volume_hud: bool,
    pub privacy_indicators: bool,
    pub notifications: bool,
    pub calendar: bool,
    /// Minutes before a meeting that it appears in the island.
    pub calendar_lead: u32,
    /// Subscribed calendar links (.ics / webcal), separated by spaces or commas.
    pub calendar_ics: String,
    pub bluetooth: bool,
    pub screenshots: bool,
    pub downloads: bool,
    /// Caps Lock and Do Not Disturb changes.
    pub status_alerts: bool,
    /// Show what Claude Code is doing (needs "Connect Claude Code" in Settings).
    pub claude_code: bool,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct DockApp {
    pub name: Option<String>,
    pub path: String,
    pub args: Option<String>,
    pub icon: Option<String>,
    #[serde(rename = "match")]
    pub match_exe: Option<String>,
}

fn appdata() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into())
}

pub fn dir() -> PathBuf {
    appdata().join("DockSide")
}

/// Carries settings over from before the rename to DockSide (`%APPDATA%\MacFinder`).
pub fn migrate_legacy() {
    let old = appdata().join("MacFinder");
    if old.is_dir() && !dir().exists() && std::fs::rename(&old, dir()).is_ok() {
        let _ = std::fs::rename(dir().join("macfinder.log"), dir().join("dockside.log"));
    }
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

/// Writes the commented default config on first run.
pub fn ensure_exists() -> Result<PathBuf> {
    let p = path();
    if !p.exists() {
        std::fs::create_dir_all(dir())?;
        std::fs::write(&p, DEFAULT_TOML)?;
    }
    Ok(p)
}

pub fn defaults() -> Config {
    parse(DEFAULT_TOML).expect("built-in default config is valid")
}

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text)
}

fn parse(user: &str) -> Result<Config> {
    let mut base: toml::Value = toml::from_str(DEFAULT_TOML)?;
    let over: toml::Value = toml::from_str(user).context("config.toml is not valid TOML")?;
    merge(&mut base, over);
    let mut cfg: Config = base.try_into().context("config.toml has an invalid value")?;
    for app in &mut cfg.dock.apps {
        app.path = expand_env(&app.path);
        if app.path == crate::finder::LEGACY_FINDER_PATH {
            app.path = crate::finder::FINDER_PATH.to_string();
        }
        app.icon = app.icon.as_deref().map(expand_env);
    }
    if cfg.menubar.logo.contains('%') {
        cfg.menubar.logo = expand_env(&cfg.menubar.logo);
    }
    Ok(cfg)
}

/// Tables merge recursively; everything else (including arrays) is replaced.
fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Watches the config directory and posts `msg` to `hwnd` when config.toml changes.
pub fn watch(path: &Path, hwnd: isize, msg: u32) -> notify::Result<notify::RecommendedWatcher> {
    let file_name = path.file_name().map(|f| f.to_owned());
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if event.kind.is_access() {
            return;
        }
        if event.paths.iter().any(|p| p.file_name() == file_name.as_deref()) {
            crate::win::post(hwnd, msg);
        }
    })?;
    watcher.watch(path.parent().unwrap_or(Path::new(".")), RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Rewrites the `[[dock.apps]]` array, keeping the rest of the file (and its comments) intact.
pub fn save_dock_apps(path: &Path, apps: &[DockApp]) -> Result<()> {
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    if !doc.contains_table("dock") {
        doc["dock"] = Item::Table(Table::new());
    }
    let mut arr = ArrayOfTables::new();
    for app in apps {
        let mut t = Table::new();
        if let Some(n) = &app.name {
            t["name"] = value(n.as_str());
        }
        t["path"] = value(app.path.as_str());
        if let Some(a) = &app.args {
            t["args"] = value(a.as_str());
        }
        if let Some(i) = &app.icon {
            t["icon"] = value(i.as_str());
        }
        if let Some(m) = &app.match_exe {
            t["match"] = value(m.as_str());
        }
        arr.push(t);
    }
    doc["dock"]["apps"] = Item::ArrayOfTables(arr);
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// A scalar setting value written by the settings UI.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigValue {
    Bool(bool),
    Int(i64),
    Str(String),
}

/// Sets `[table] key = value`, keeping the rest of the file (and comments) intact.
pub fn save_value(path: &Path, table: &str, key: &str, value: &ConfigValue) -> Result<()> {
    save_values(path, table, &[(key, value.clone())])
}

/// Sets several keys of one table in a single write.
pub fn save_values(path: &Path, table: &str, values: &[(&str, ConfigValue)]) -> Result<()> {
    use toml_edit::{DocumentMut, Item, Table};

    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    if !doc.contains_table(table) {
        doc[table] = Item::Table(Table::new());
    }
    for (key, value) in values {
        doc[table][*key] = match value {
            ConfigValue::Bool(b) => toml_edit::value(*b),
            ConfigValue::Int(i) => toml_edit::value(*i),
            ConfigValue::Str(s) => toml_edit::value(s.as_str()),
        };
    }
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let cfg = defaults();
        assert!(cfg.dock.apps.len() >= 3);
        assert_eq!(cfg.menubar.height, 24.0);
        assert!(cfg.island.enabled);
    }

    #[test]
    fn partial_override_keeps_defaults() {
        let cfg = parse("[dock]\ntile_size = 64\n").unwrap();
        assert_eq!(cfg.dock.tile_size, 64.0);
        assert!(cfg.dock.magnification);
        assert!(!cfg.dock.apps.is_empty());
    }

    #[test]
    fn finder_prefs_parse() {
        let cfg = parse("[finder]
view = \"columns\"
sort = \"modified\"
").unwrap();
        assert_eq!(cfg.finder.view, crate::finder::View::Columns);
        assert_eq!(cfg.finder.sort, crate::finder::fs::SortKey::Modified);
        assert!(cfg.finder.ascending);
    }

    #[test]
    fn env_expansion() {
        std::env::set_var("MF_TEST_VAR", "X");
        assert_eq!(expand_env("a%MF_TEST_VAR%b"), "aXb");
        assert_eq!(expand_env("100%"), "100%");
    }
}
