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
    pub desktops: DesktopsCfg,
    pub widgets: WidgetsCfg,
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
    /// "glass" backdrops bend what's behind them (hides them from screen capture).
    pub glass_refraction: bool,
    /// The Setup Assistant was finished or skipped (it opens on launch until then).
    pub onboarding_done: bool,
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
    /// Liquid Glass: a clear, lightly tinted blur with a sheen and specular rim.
    Glass,
    /// No bar at all (menu bar only): text straight on the wallpaper, like macOS Tahoe.
    Clear,
    None,
}

impl Backdrop {
    /// The material for menus and Spotlight, which follow the menu bar's
    /// setting but always need a body: a clear bar's panels are glass.
    pub fn for_panels(self) -> Backdrop {
        if self == Backdrop::Clear { Backdrop::Glass } else { self }
    }
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IconStyle {
    Native,
    Squircle,
}

/// Shape of the Dynamic Island.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IslandStyle {
    /// A floating capsule, like the iPhone.
    Pill,
    /// Hangs from the top edge with a flat top and rounded bottom corners,
    /// like a MacBook notch; panels open as squarer cards.
    Notch,
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
    /// An item listing every app's notification-area (tray) icon.
    pub show_tray: bool,
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
    /// Gap between items, as a percentage of the tile size.
    pub item_spacing: f32,
    pub backdrop: Backdrop,
    pub tint_light: String,
    pub tint_dark: String,
    pub icon_style: IconStyle,
    pub show_running: bool,
    pub show_trash: bool,
    /// A soft drop shadow under the shelf.
    pub shadow: bool,
    pub autohide: bool,
    pub show_on_desktop: bool,
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
    /// Sidebar material. Anything but "none" floats it as a rounded pane, like macOS Tahoe.
    pub sidebar: Backdrop,
}

/// Virtual desktop shortcuts.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct DesktopsCfg {
    /// Alt+1…9 switches to that desktop.
    pub alt_number: bool,
    /// Win+Ctrl+←/→ switch without Windows' desktop popup.
    pub replace_windows_switcher: bool,
    /// Show switches in the Dynamic Island.
    pub island: bool,
}

/// The Dynamic Island at the top center of the screen.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct IslandCfg {
    pub enabled: bool,
    /// Keep the idle pill visible when nothing is happening.
    pub always_show: bool,
    pub expand_on_hover: bool,
    pub style: IslandStyle,
    /// Points to move the island down (negative moves it up).
    pub vertical_offset: f32,
    /// Seconds paused media stays in the island (0 = until the app ends its session).
    pub hide_paused_after: u32,

    // Activities: things that stay in the island while they're happening.
    /// Media playing in any app.
    pub now_playing: bool,
    /// Timers and the stopwatch (the home panel's buttons and the right-click menu).
    pub timers: bool,
    /// Browser downloads in progress.
    pub downloads: bool,
    pub calendar: bool,
    /// Minutes before a meeting that it appears in the island.
    pub calendar_lead: u32,
    /// Subscribed calendar links (.ics / webcal), separated by spaces or commas.
    pub calendar_ics: String,
    /// Show what Claude Code is doing (needs "Connect Claude Code" in Settings).
    pub claude_code: bool,
    /// Files dropped on the island are held on a shelf.
    pub shelf: bool,

    // Alerts: brief pop-ups when something changes.
    /// Volume keys and scrolling over the island, instead of the Windows popup.
    pub volume: bool,
    /// Brightness changes, instead of the Windows popup.
    pub brightness: bool,
    pub charging: bool,
    pub low_battery: bool,
    pub caps_lock: bool,
    pub do_not_disturb: bool,
    pub notifications: bool,
    pub bluetooth: bool,
    /// An app starting to use the camera or microphone.
    pub privacy_indicators: bool,
    pub screenshots: bool,
    /// A browser download finishing.
    pub download_alerts: bool,
}

/// Desktop widgets: frosted cards that sit on the wallpaper, below every window.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct WidgetsCfg {
    pub enabled: bool,
    /// "acrylic", "blur", "glass" or "none" (solid).
    pub backdrop: Backdrop,
    /// Weather location: a place name, "lat,lon", or empty to find it from the network.
    pub location: String,
    /// "auto" (from the Windows region), "c" or "f".
    pub temperature_unit: String,
    /// World Clock zones: "Area/City" or "Label=Area/City".
    pub world_clocks: Vec<String>,
    #[serde(default)]
    pub items: Vec<WidgetItem>,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    Clock,
    WorldClock,
    Weather,
    Calendar,
    UpNext,
    Batteries,
    Notes,
    Reminders,
    NowPlaying,
    System,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WidgetSize {
    Small,
    Medium,
    Large,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct WidgetItem {
    /// Stable id (notes are stored by it); assigned on load when missing.
    #[serde(default)]
    pub id: u32,
    pub kind: WidgetKind,
    pub size: WidgetSize,
    /// Top-left corner, in points from the top-left of the screen.
    pub x: f32,
    pub y: f32,
    /// World Clock (small): the zone to show, instead of the first of `world_clocks`.
    pub zone: Option<String>,
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
    let mut over: toml::Value = toml::from_str(user).context("config.toml is not valid TOML")?;
    migrate_sidebar(&mut over);
    migrate_island(&mut over);
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
    assign_widget_ids(&mut cfg.widgets.items);
    Ok(cfg)
}

/// Widgets without an id (the defaults, hand-written ones) get the next free ones, in order.
fn assign_widget_ids(items: &mut [WidgetItem]) {
    let mut next = items.iter().map(|w| w.id).max().unwrap_or(0);
    let mut seen = std::collections::HashSet::new();
    for w in items {
        if w.id == 0 || !seen.insert(w.id) {
            next += 1;
            w.id = next;
            seen.insert(next);
        }
    }
}

/// `sidebar_vibrancy = false` (before `sidebar` existed) meant a solid sidebar.
fn migrate_sidebar(over: &mut toml::Value) {
    let Some(finder) = over.get_mut("finder").and_then(|f| f.as_table_mut()) else { return };
    if let Some(v) = finder.remove("sidebar_vibrancy") {
        if v.as_bool() == Some(false) && !finder.contains_key("sidebar") {
            finder.insert("sidebar".into(), toml::Value::String("none".into()));
        }
    }
}

/// Island switches that used to be shared now come in pairs: each half
/// starts from the old shared value.
fn migrate_island(over: &mut toml::Value) {
    let Some(island) = over.get_mut("island").and_then(|i| i.as_table_mut()) else { return };
    let splits = [
        ("volume_hud", ["volume", "brightness"]),
        ("battery_alerts", ["charging", "low_battery"]),
        ("status_alerts", ["caps_lock", "do_not_disturb"]),
    ];
    for (old, new) in splits {
        if let Some(v) = island.remove(old) {
            for key in new {
                if !island.contains_key(key) {
                    island.insert(key.into(), v.clone());
                }
            }
        }
    }
    // `downloads` covered the finished-download alert too.
    if let Some(v) = island.get("downloads").cloned() {
        if !island.contains_key("download_alerts") {
            island.insert("download_alerts".into(), v);
        }
    }
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

/// Rewrites the `[[widgets.items]]` array, keeping the rest of the file intact.
pub fn save_widgets(path: &Path, items: &[WidgetItem]) -> Result<()> {
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    if !doc.contains_table("widgets") {
        doc["widgets"] = Item::Table(Table::new());
    }
    let name = |s: &dyn std::fmt::Debug| {
        // WidgetKind::WorldClock -> "world_clock", WidgetSize::Small -> "small".
        let mut out = String::new();
        for (i, c) in format!("{s:?}").chars().enumerate() {
            if c.is_uppercase() && i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        }
        out
    };
    let mut arr = ArrayOfTables::new();
    for w in items {
        let mut t = Table::new();
        t["id"] = value(w.id as i64);
        t["kind"] = value(name(&w.kind));
        t["size"] = value(name(&w.size));
        t["x"] = value(w.x.round() as i64);
        t["y"] = value(w.y.round() as i64);
        if let Some(z) = &w.zone {
            t["zone"] = value(z.as_str());
        }
        arr.push(t);
    }
    doc["widgets"]["items"] = Item::ArrayOfTables(arr);
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// A scalar setting value written by the settings UI.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigValue {
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<String>),
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
            ConfigValue::List(l) => toml_edit::value(l.iter().map(String::as_str).collect::<toml_edit::Array>()),
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
        assert!(!cfg.general.onboarding_done);
    }

    #[test]
    fn old_island_switches_split() {
        let cfg = parse("[island]\nvolume_hud = false\nstatus_alerts = false\ndownloads = false\ncaps_lock = true\n").unwrap();
        let i = &cfg.island;
        assert!(!i.volume && !i.brightness);
        // A new key already set wins over the old shared one.
        assert!(i.caps_lock && !i.do_not_disturb);
        assert!(!i.downloads && !i.download_alerts);
        assert!(i.charging && i.low_battery);
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
    fn sidebar_vibrancy_migrates() {
        assert_eq!(parse("[finder]\nsidebar_vibrancy = false\n").unwrap().finder.sidebar, Backdrop::None);
        assert_eq!(parse("[finder]\nsidebar_vibrancy = true\n").unwrap().finder.sidebar, Backdrop::Glass);
        let cfg = parse("[finder]\nsidebar_vibrancy = false\nsidebar = \"acrylic\"\n").unwrap();
        assert_eq!(cfg.finder.sidebar, Backdrop::Acrylic);
    }

    #[test]
    fn widgets_parse_and_get_ids() {
        let cfg = defaults();
        assert!(!cfg.widgets.items.is_empty());
        let ids: Vec<u32> = cfg.widgets.items.iter().map(|w| w.id).collect();
        assert!(ids.iter().all(|&i| i > 0));
        let cfg = parse("[[widgets.items]]
id = 5
kind = \"world_clock\"
size = \"medium\"
x = 1
y = 2
[[widgets.items]]
kind = \"notes\"
size = \"small\"
x = 3
y = 4
").unwrap();
        assert_eq!(cfg.widgets.items[0].kind, WidgetKind::WorldClock);
        assert_eq!(cfg.widgets.items[1].id, 6);
    }

    #[test]
    fn widgets_round_trip() {
        let dir = std::env::temp_dir().join(format!("dockside-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# keep me
[widgets]
enabled = true
").unwrap();
        let items = defaults().widgets.items;
        save_widgets(&path, &items).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep me"));
        assert_eq!(parse(&text).unwrap().widgets.items, items);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_expansion() {
        std::env::set_var("MF_TEST_VAR", "X");
        assert_eq!(expand_env("a%MF_TEST_VAR%b"), "aXb");
        assert_eq!(expand_env("100%"), "100%");
    }
}
