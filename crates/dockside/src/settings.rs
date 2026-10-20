//! A macOS System Settings-style window: sidebar of pages, grouped cards of
//! rows with toggles, segmented controls, sliders and accent swatches.
//!
//! Every control writes `config.toml` (via [`Action::SetConfig`]) and the
//! change is applied immediately, so the file stays the single source of truth.

use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{w, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::chrome::{self, SHADOW};
use crate::config::{Appearance, Backdrop, Config, ConfigValue, IconStyle, ShortcutStyle};
use crate::finder::text::TextField;
use crate::geom::{Color, Rect};
use crate::gfx::icons::IconStore;
use crate::gfx::{Align, Canvas, Gfx, Painter, Weight};
use crate::menu::Action;
use crate::spotlight::web;
use crate::theme::Theme;
use crate::win::{self, startup, Monitor};

/// Lets the shared window procedure let this window activate (all others don't).
pub static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

const W: f32 = 780.0;
const H: f32 = 560.0;
const SIDEBAR: f32 = 210.0;
const TITLE_H: f32 = 52.0;
const ROW_H: f32 = 40.0;
const CONTENT_PAD: f32 = 24.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    General,
    Appearance,
    MenuBar,
    Dock,
    Finder,
    Desktops,
    Island,
}

const PAGES: [(Page, &str); 7] = [
    (Page::General, "General"),
    (Page::Appearance, "Appearance"),
    (Page::MenuBar, "Menu Bar"),
    (Page::Island, "Dynamic Island"),
    (Page::Dock, "Dock"),
    (Page::Finder, "Finder"),
    (Page::Desktops, "Desktops"),
];

/// macOS accent colors: blue, purple, pink, red, orange, yellow, green, graphite.
const ACCENTS: [&str; 8] = ["#007AFF", "#A550A7", "#F74F9E", "#FF5257", "#F7821B", "#FFC600", "#62BA46", "#8C8C8C"];

/// The `[island]` on/off setting called `name`.
fn island_flag(c: &crate::config::IslandCfg, name: &str) -> bool {
    match name {
        "enabled" => c.enabled,
        "always_show" => c.always_show,
        "expand_on_hover" => c.expand_on_hover,
        "now_playing" => c.now_playing,
        "timers" => c.timers,
        "downloads" => c.downloads,
        "calendar" => c.calendar,
        "claude_code" => c.claude_code,
        "shelf" => c.shelf,
        "volume" => c.volume,
        "brightness" => c.brightness,
        "charging" => c.charging,
        "low_battery" => c.low_battery,
        "caps_lock" => c.caps_lock,
        "do_not_disturb" => c.do_not_disturb,
        "notifications" => c.notifications,
        "bluetooth" => c.bluetooth,
        "privacy_indicators" => c.privacy_indicators,
        "screenshots" => c.screenshots,
        "download_alerts" => c.download_alerts,
        _ => false,
    }
}

const CLOCKS: [(&str, &str); 4] = [
    ("9:41 AM", "%-I:%M %p"),
    ("Fri 9:41 AM", "%a %-I:%M %p"),
    ("Fri Oct 17 9:41 AM", "%a %b %-d  %-I:%M %p"),
    ("24-hour", "%a %b %-d  %H:%M"),
];

const FONT_CHOICES: [&str; 4] = ["SF Pro Text", "SF Pro", "Inter", "Segoe UI Variable Text"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    StartAtLogin,
    HideTaskbar,
    ShortcutStyle,
    Appearance,
    Font,
    GlassRefraction,
    FinderSidebar,
    FinderView,
    FinderHidden,
    DesktopsAlt,
    DesktopsArrows,
    DesktopsIsland,
    MbEnabled,
    MbBackdrop,
    MbHeight,
    MbLogo,
    MbClock,
    MbReserve,
    MbBattery,
    MbBatteryPct,
    MbWifi,
    MbSearch,
    MbControlCenter,
    MbTray,
    /// An on/off setting in `[island]`, by name.
    Island(&'static str),
    IslandIcs,
    DockEnabled,
    DockSize,
    DockMag,
    DockMagSize,
    DockOffset,
    DockAutohide,
    DockIconStyle,
    DockBackdrop,
    DockRunning,
    DockTrash,
    SpotlightEnabled,
    SpotlightHotkey,
    SearchEngine,
    CustomSearchUrl,
}

impl Key {
    fn path(self) -> (&'static str, &'static str) {
        match self {
            Key::StartAtLogin => ("", ""),
            Key::HideTaskbar => ("general", "hide_taskbar"),
            Key::ShortcutStyle => ("general", "shortcut_style"),
            Key::Appearance => ("general", "appearance"),
            Key::Font => ("general", "font"),
            Key::GlassRefraction => ("general", "glass_refraction"),
            Key::FinderSidebar => ("finder", "sidebar"),
            Key::FinderView => ("finder", "view"),
            Key::FinderHidden => ("finder", "show_hidden"),
            Key::DesktopsAlt => ("desktops", "alt_number"),
            Key::DesktopsArrows => ("desktops", "replace_windows_switcher"),
            Key::DesktopsIsland => ("desktops", "island"),
            Key::MbEnabled => ("menubar", "enabled"),
            Key::MbBackdrop => ("menubar", "backdrop"),
            Key::MbHeight => ("menubar", "height"),
            Key::MbLogo => ("menubar", "logo"),
            Key::MbClock => ("menubar", "clock_format"),
            Key::MbReserve => ("menubar", "reserve_space"),
            Key::MbBattery => ("menubar", "show_battery"),
            Key::MbBatteryPct => ("menubar", "show_battery_percent"),
            Key::MbWifi => ("menubar", "show_wifi"),
            Key::MbSearch => ("menubar", "show_search"),
            Key::MbControlCenter => ("menubar", "show_control_center"),
            Key::MbTray => ("menubar", "show_tray"),
            Key::Island(name) => ("island", name),
            Key::IslandIcs => ("island", "calendar_ics"),
            Key::DockEnabled => ("dock", "enabled"),
            Key::DockSize => ("dock", "tile_size"),
            Key::DockMag => ("dock", "magnification"),
            Key::DockMagSize => ("dock", "magnified_size"),
            Key::DockOffset => ("dock", "bottom_margin"),
            Key::DockAutohide => ("dock", "autohide"),
            Key::DockIconStyle => ("dock", "icon_style"),
            Key::DockBackdrop => ("dock", "backdrop"),
            Key::DockRunning => ("dock", "show_running"),
            Key::DockTrash => ("dock", "show_trash"),
            Key::SpotlightEnabled => ("spotlight", "enabled"),
            Key::SpotlightHotkey => ("spotlight", "hotkey"),
            Key::SearchEngine => ("spotlight", "search_engine"),
            Key::CustomSearchUrl => ("spotlight", "custom_search_url"),
        }
    }

    fn set(self, value: ConfigValue) -> Action {
        let (table, key) = self.path();
        Action::SetConfig { table, key, value }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Close,
    Minimize,
    Page(Page),
    Toggle(Key),
    Segment(Key, usize),
    Slider(Key),
    Swatch(usize),
    Remove(usize),
    AddApp,
    OpenConfig,
    OpenLog,
    Field(Key),
    ConnectClaude(bool),
}

enum Row {
    Toggle(&'static str, Key),
    Segmented(&'static str, Key),
    Slider(&'static str, Key),
    Swatches(&'static str),
    App(usize),
    Buttons(&'static str, Vec<(&'static str, Hit)>),
    Info(&'static str, String),
    /// Label, key, placeholder.
    Text(&'static str, Key, &'static str),
}

struct Group {
    title: Option<&'static str>,
    rows: Vec<Row>,
}

pub enum SettingsEvent {
    None,
    Action(Action),
    Close,
}

/// A text row being edited.
struct Editing {
    key: Key,
    field: TextField,
    /// Last commit was rejected (shown with a red outline).
    invalid: bool,
}

#[derive(Clone, Copy)]
struct SliderDrag {
    key: Key,
    track: Rect,
    min: f32,
    max: f32,
    value: f32,
}

pub struct SettingsWindow {
    pub hwnd: HWND,
    canvas: Canvas,
    scale: f32,
    page: Page,
    scroll: f32,
    max_scroll: f32,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    slider: Option<SliderDrag>,
    editing: Option<Editing>,
    bitmaps: HashMap<String, Option<ID2D1Bitmap>>,
    login: bool,
    fonts: Vec<String>,
    shown: bool,
    pub tracking: bool,
    pub dirty: bool,
}

impl SettingsWindow {
    pub fn new(gfx: &Gfx, monitor: &Monitor, page: Page) -> Result<Self> {
        let s = monitor.scale;
        let (w, h) = (((W + 2.0 * SHADOW) * s).round() as i32, ((H + 2.0 * SHADOW) * s).round() as i32);
        let mon = monitor.rect;
        let left = mon.left + (mon.right - mon.left - w) / 2;
        let top = mon.top + (mon.bottom - mon.top - h) / 2;
        let rect = RECT { left, top, right: left + w, bottom: top + h };
        let hwnd = win::create_window(WS_EX_LAYERED | WS_EX_APPWINDOW, WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX, rect, HWND::default())?;
        unsafe {
            let _ = SetWindowTextW(hwnd, w!("System Settings"));
        }
        SETTINGS_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        let fonts = FONT_CHOICES.iter().filter(|f| gfx.font_exists(f)).map(|f| f.to_string()).collect();
        Ok(Self {
            hwnd,
            canvas: Canvas::new(gfx, w, h, s)?,
            scale: s,
            page,
            scroll: 0.0,
            max_scroll: 0.0,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            slider: None,
            editing: None,
            bitmaps: HashMap::new(),
            login: startup::is_enabled(),
            fonts,
            shown: false,
            tracking: false,
            dirty: true,
        })
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn show_page(&mut self, page: Option<Page>) {
        if let Some(p) = page {
            if p != self.page {
                self.page = p;
                self.scroll = 0.0;
                self.dirty = true;
            }
        }
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    pub fn minimize(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
        }
    }

    fn body() -> Rect {
        Rect::new(SHADOW, SHADOW, W, H)
    }

    /// Title bar area (window-local points) that drags the window.
    pub fn is_caption(&self, x: f32, y: f32) -> bool {
        let b = Self::body();
        let title = Rect::new(b.x, b.y, b.w, TITLE_H);
        title.contains(x, y) && self.hit_at(x, y).is_none()
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    fn groups(&self, cfg: &Config) -> Vec<Group> {
        match self.page {
            Page::General => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Toggle("Start DockSide at login", Key::StartAtLogin),
                        Row::Toggle("Hide the Windows taskbar", Key::HideTaskbar),
                        Row::Segmented("Shortcut hints in menus", Key::ShortcutStyle),
                    ],
                },
                Group {
                    title: Some("Spotlight"),
                    rows: {
                        let mut rows = vec![
                            Row::Toggle("Spotlight search", Key::SpotlightEnabled),
                            Row::Segmented("Keyboard shortcut", Key::SpotlightHotkey),
                            Row::Segmented("Web search", Key::SearchEngine),
                        ];
                        if cfg.spotlight.search_engine.eq_ignore_ascii_case("custom") {
                            rows.push(Row::Text("Search URL", Key::CustomSearchUrl, "https://example.com/search?q=%s"));
                        }
                        rows
                    },
                },
                Group {
                    title: Some("Configuration"),
                    rows: vec![Row::Buttons(
                        "Settings file",
                        vec![("Show Log", Hit::OpenLog), ("Open config.toml", Hit::OpenConfig)],
                    )],
                },
                Group {
                    title: Some("About"),
                    rows: vec![
                        Row::Info("Version", env!("CARGO_PKG_VERSION").to_string()),
                        Row::Info("Settings folder", crate::config::dir().display().to_string()),
                    ],
                },
            ],
            Page::Appearance => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Segmented("Appearance", Key::Appearance),
                        Row::Swatches("Accent color"),
                        Row::Segmented("Font", Key::Font),
                    ],
                },
                Group {
                    title: Some("Liquid Glass"),
                    rows: vec![
                        // Refraction hides glass surfaces from screenshots and screen sharing.
                        Row::Toggle("Refraction (hidden from screenshots)", Key::GlassRefraction),
                    ],
                },
            ],
            Page::MenuBar => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Toggle("Show menu bar", Key::MbEnabled),
                        Row::Segmented("Background", Key::MbBackdrop),
                        Row::Slider("Size", Key::MbHeight),
                        Row::Segmented("Logo", Key::MbLogo),
                        Row::Segmented("Clock", Key::MbClock),
                        Row::Toggle("Keep windows below the menu bar", Key::MbReserve),
                    ],
                },
                Group {
                    title: Some("Status items"),
                    rows: vec![
                        Row::Toggle("Battery", Key::MbBattery),
                        Row::Toggle("Battery percentage", Key::MbBatteryPct),
                        Row::Toggle("Wi-Fi", Key::MbWifi),
                        Row::Toggle("Search", Key::MbSearch),
                        Row::Toggle("Control Center", Key::MbControlCenter),
                        Row::Toggle("Tray icons", Key::MbTray),
                    ],
                },
            ],
            Page::Island => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Toggle("Show Dynamic Island", Key::Island("enabled")),
                        Row::Toggle("Always show the island", Key::Island("always_show")),
                        Row::Toggle("Expand when hovered", Key::Island("expand_on_hover")),
                    ],
                },
                Group {
                    title: Some("Activities"),
                    rows: vec![
                        Row::Toggle("Now Playing", Key::Island("now_playing")),
                        Row::Toggle("Timers and stopwatch", Key::Island("timers")),
                        Row::Toggle("Downloads in progress", Key::Island("downloads")),
                        Row::Toggle("Upcoming meetings", Key::Island("calendar")),
                        Row::Text("Calendar link", Key::IslandIcs, "https://…/basic.ics or webcal://…"),
                        Row::Toggle("Claude Code", Key::Island("claude_code")),
                        Row::Toggle("File shelf", Key::Island("shelf")),
                    ],
                },
                Group {
                    title: Some("Alerts"),
                    rows: vec![
                        Row::Toggle("Volume", Key::Island("volume")),
                        Row::Toggle("Brightness", Key::Island("brightness")),
                        Row::Toggle("Charging", Key::Island("charging")),
                        Row::Toggle("Low battery", Key::Island("low_battery")),
                        Row::Toggle("Caps Lock", Key::Island("caps_lock")),
                        Row::Toggle("Do Not Disturb", Key::Island("do_not_disturb")),
                        Row::Toggle("Notifications", Key::Island("notifications")),
                        Row::Toggle("Bluetooth devices", Key::Island("bluetooth")),
                        Row::Toggle("Camera and microphone", Key::Island("privacy_indicators")),
                        Row::Toggle("Screenshots", Key::Island("screenshots")),
                        Row::Toggle("Finished downloads", Key::Island("download_alerts")),
                        Row::Toggle("Desktop switches", Key::DesktopsIsland),
                    ],
                },
                Group {
                    title: Some("Claude Code"),
                    rows: if crate::island::claude::connected() {
                        vec![
                            Row::Info("Status", "Connected: sessions show in the island".to_string()),
                            Row::Buttons("", vec![("Disconnect", Hit::ConnectClaude(false)), ("Reconnect", Hit::ConnectClaude(true))]),
                        ]
                    } else {
                        vec![
                            Row::Info("Status", "Not connected".to_string()),
                            Row::Buttons("", vec![("Connect Claude Code", Hit::ConnectClaude(true))]),
                        ]
                    },
                },
            ],
            Page::Dock => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Toggle("Show Dock", Key::DockEnabled),
                        Row::Slider("Size", Key::DockSize),
                        Row::Toggle("Magnification", Key::DockMag),
                        Row::Slider("Magnified size", Key::DockMagSize),
                        Row::Slider("Vertical offset", Key::DockOffset),
                        Row::Toggle("Automatically hide and show the Dock", Key::DockAutohide),
                        Row::Segmented("Icon style", Key::DockIconStyle),
                        Row::Segmented("Background", Key::DockBackdrop),
                        Row::Toggle("Show running apps", Key::DockRunning),
                        Row::Toggle("Show Trash", Key::DockTrash),
                    ],
                },
                Group {
                    title: Some("Pinned apps"),
                    rows: (0..cfg.dock.apps.len())
                        .map(Row::App)
                        .chain(std::iter::once(Row::Buttons("", vec![("Add App…", Hit::AddApp)])))
                        .collect(),
                },
            ],
            Page::Finder => vec![
                Group {
                    title: None,
                    rows: vec![
                        Row::Segmented("Sidebar", Key::FinderSidebar),
                        Row::Segmented("New windows open in", Key::FinderView),
                        Row::Toggle("Show hidden files", Key::FinderHidden),
                    ],
                },
            ],
            Page::Desktops => vec![Group {
                title: None,
                rows: vec![
                    Row::Toggle("Alt+1 … Alt+9 switch desktops", Key::DesktopsAlt),
                    Row::Toggle("Win+Ctrl+← / → without the Windows popup", Key::DesktopsArrows),
                    Row::Toggle("Show switches in the Dynamic Island", Key::DesktopsIsland),
                ],
            }],
        }
    }

    fn toggle_on(login: bool, key: Key, cfg: &Config) -> bool {
        let (g, m, d) = (&cfg.general, &cfg.menubar, &cfg.dock);
        match key {
            Key::StartAtLogin => login,
            Key::HideTaskbar => g.hide_taskbar,
            Key::GlassRefraction => g.glass_refraction,
            Key::FinderHidden => cfg.finder.show_hidden,
            Key::DesktopsAlt => cfg.desktops.alt_number,
            Key::DesktopsArrows => cfg.desktops.replace_windows_switcher,
            Key::DesktopsIsland => cfg.desktops.island,
            Key::MbEnabled => m.enabled,
            Key::MbReserve => m.reserve_space,
            Key::MbBattery => m.show_battery,
            Key::MbBatteryPct => m.show_battery_percent,
            Key::MbWifi => m.show_wifi,
            Key::MbSearch => m.show_search,
            Key::MbControlCenter => m.show_control_center,
            Key::MbTray => m.show_tray,
            Key::Island(name) => island_flag(&cfg.island, name),
            Key::DockEnabled => d.enabled,
            Key::DockMag => d.magnification,
            Key::DockAutohide => d.autohide,
            Key::DockRunning => d.show_running,
            Key::DockTrash => d.show_trash,
            Key::SpotlightEnabled => cfg.spotlight.enabled,
            _ => false,
        }
    }

    /// (labels, values, selected index) for a segmented control.
    fn segments(fonts: &[String], key: Key, cfg: &Config) -> (Vec<String>, Vec<String>, Option<usize>) {
        let backdrop = |b: Backdrop| match b {
            Backdrop::Acrylic => 0,
            Backdrop::Blur => 1,
            Backdrop::Glass => 2,
            Backdrop::None => 3,
            Backdrop::Clear => 4,
        };
        let fixed = |pairs: &[(&str, &str)], sel: Option<usize>| {
            (pairs.iter().map(|p| p.0.to_string()).collect(), pairs.iter().map(|p| p.1.to_string()).collect(), sel)
        };
        const BACKDROPS: [(&str, &str); 4] =
            [("Acrylic", "acrylic"), ("Blur", "blur"), ("Glass", "glass"), ("None", "none")];
        const MB_BACKDROPS: [(&str, &str); 5] =
            [("Acrylic", "acrylic"), ("Blur", "blur"), ("Glass", "glass"), ("None", "none"), ("Clear", "clear")];
        match key {
            Key::ShortcutStyle => fixed(
                &[("Mac  ⌘C", "mac"), ("Windows  Ctrl+C", "windows")],
                Some((cfg.general.shortcut_style == ShortcutStyle::Windows) as usize),
            ),
            Key::Appearance => fixed(
                &[("Auto", "auto"), ("Light", "light"), ("Dark", "dark")],
                Some(match cfg.general.appearance {
                    Appearance::Auto => 0,
                    Appearance::Light => 1,
                    Appearance::Dark => 2,
                }),
            ),
            Key::Font => {
                let mut values = fonts.to_vec();
                let mut labels = fonts.to_vec();
                // The configured font is listed even when missing (the UI falls back), but say so.
                if !values.contains(&cfg.general.font) {
                    values.push(cfg.general.font.clone());
                    labels.push(format!("{} (not installed)", cfg.general.font));
                }
                let sel = values.iter().position(|f| *f == cfg.general.font);
                (labels, values, sel)
            }
            Key::FinderSidebar => fixed(
                &[("Glass", "glass"), ("Acrylic", "acrylic"), ("Blur", "blur"), ("Solid", "none")],
                Some(match cfg.finder.sidebar {
                    Backdrop::Glass => 0,
                    Backdrop::Acrylic => 1,
                    Backdrop::Blur => 2,
                    Backdrop::None | Backdrop::Clear => 3,
                }),
            ),
            Key::FinderView => fixed(
                &[("Icons", "icons"), ("List", "list"), ("Columns", "columns")],
                Some(match cfg.finder.view {
                    crate::finder::View::Icons => 0,
                    crate::finder::View::List => 1,
                    crate::finder::View::Columns => 2,
                }),
            ),
            Key::MbBackdrop => fixed(&MB_BACKDROPS, Some(backdrop(cfg.menubar.backdrop))),
            Key::DockBackdrop => fixed(&BACKDROPS, Some(backdrop(cfg.dock.backdrop)).filter(|&i| i < BACKDROPS.len())),
            Key::MbLogo => {
                let sel = match cfg.menubar.logo.to_lowercase().as_str() {
                    "apple" => Some(0),
                    "windows" => Some(1),
                    _ => None,
                };
                fixed(&[("Apple", "apple"), ("Windows", "windows")], sel)
            }
            Key::MbClock => fixed(&CLOCKS, CLOCKS.iter().position(|c| c.1 == cfg.menubar.clock_format)),
            Key::SpotlightHotkey => {
                const KEYS: [(&str, &str); 3] =
                    [("Alt+Space", "alt+space"), ("Ctrl+Space", "ctrl+space"), ("Ctrl+Alt+Space", "ctrl+alt+space")];
                let current = cfg.spotlight.hotkey.to_lowercase().replace(' ', "");
                fixed(&KEYS, KEYS.iter().position(|k| k.1 == current))
            }
            Key::SearchEngine => {
                let mut labels: Vec<String> = web::ENGINES.iter().map(|e| e.1.to_string()).collect();
                let mut values: Vec<String> = web::ENGINES.iter().map(|e| e.0.to_string()).collect();
                labels.push("Custom".into());
                values.push("custom".into());
                let sel = values.iter().position(|v| v.eq_ignore_ascii_case(&cfg.spotlight.search_engine));
                (labels, values, sel)
            }
            Key::DockIconStyle => fixed(
                &[("Native", "native"), ("Squircle", "squircle")],
                Some((cfg.dock.icon_style == IconStyle::Squircle) as usize),
            ),
            _ => (vec![], vec![], None),
        }
    }

    fn text_value(key: Key, cfg: &Config) -> String {
        match key {
            Key::CustomSearchUrl => cfg.spotlight.custom_search_url.clone(),
            Key::IslandIcs => cfg.island.calendar_ics.clone(),
            _ => String::new(),
        }
    }

    /// Finishes editing a text row: an action when the value changed, nothing
    /// when unchanged, and keeps editing (marked invalid) when rejected.
    fn commit(&mut self, cfg: &Config) -> SettingsEvent {
        let Some(ed) = &mut self.editing else { return SettingsEvent::None };
        let text = ed.field.string().trim().to_string();
        let bad_link = ed.key == Key::IslandIcs && !crate::island::calendar_links_valid(&text);
        if bad_link || ed.key == Key::CustomSearchUrl && !text.is_empty() && !web::valid_template(&text) {
            ed.invalid = true;
            self.dirty = true;
            return SettingsEvent::None;
        }
        let key = ed.key;
        self.editing = None;
        self.dirty = true;
        if text == Self::text_value(key, cfg) {
            return SettingsEvent::None;
        }
        SettingsEvent::Action(key.set(ConfigValue::Str(text)))
    }

    /// Keyboard input while the window is focused.
    pub fn on_key(&mut self, vk: u16, ctrl: bool, shift: bool, cfg: &Config) -> SettingsEvent {
        const ENTER: u16 = 0x0D;
        const ESC: u16 = 0x1B;
        let Some(ed) = &mut self.editing else {
            return if vk == ESC { SettingsEvent::Close } else { SettingsEvent::None };
        };
        self.dirty = true;
        match vk {
            ENTER => self.commit(cfg),
            ESC => {
                self.editing = None;
                SettingsEvent::None
            }
            _ => {
                if ed.field.on_key(vk, ctrl, shift) == crate::finder::text::KeyResult::Edited {
                    ed.invalid = false;
                }
                SettingsEvent::None
            }
        }
    }

    pub fn on_char(&mut self, c: char) {
        if let Some(ed) = self.editing.as_mut().filter(|_| !c.is_control()) {
            ed.field.insert(&c.to_string());
            ed.invalid = false;
            self.dirty = true;
        }
    }

    fn slider_spec(key: Key, cfg: &Config) -> (f32, f32, f32) {
        match key {
            Key::DockSize => (cfg.dock.tile_size, 24.0, 96.0),
            Key::MbHeight => (cfg.menubar.height, 22.0, 40.0),
            Key::DockMagSize => (cfg.dock.magnified_size, 48.0, 160.0),
            Key::DockOffset => (cfg.dock.bottom_margin, 0.0, 120.0),
            _ => (0.0, 0.0, 1.0),
        }
    }

    // --- input -------------------------------------------------------------

    pub fn on_move(&mut self, x: f32, y: f32) {
        if let Some(s) = &mut self.slider {
            let t = ((x - s.track.x) / s.track.w).clamp(0.0, 1.0);
            s.value = (s.min + t * (s.max - s.min)).round();
            self.dirty = true;
            return;
        }
        let h = self.hit_at(x, y);
        if h != self.hover {
            self.hover = h;
            self.dirty = true;
        }
    }

    pub fn on_leave(&mut self) {
        self.tracking = false;
        if self.hover.take().is_some() {
            self.dirty = true;
        }
    }

    pub fn on_wheel(&mut self, delta: i32) {
        let s = (self.scroll - delta as f32 / 120.0 * 48.0).clamp(0.0, self.max_scroll);
        if s != self.scroll {
            self.scroll = s;
            self.dirty = true;
        }
    }

    pub fn on_down(&mut self, x: f32, y: f32, cfg: &Config) {
        self.pressed = self.hit_at(x, y);
        if let Some(Hit::Slider(key)) = self.pressed {
            if let Some((track, _)) = self.hits.iter().find(|(_, h)| *h == Hit::Slider(key)) {
                let (value, min, max) = Self::slider_spec(key, cfg);
                // The hit rect is the track grown vertically; recover the track.
                let track = Rect::new(track.x + 9.0, track.y, track.w - 18.0, track.h);
                self.slider = Some(SliderDrag { key, track, min, max, value });
                self.on_move(x, y);
            }
        }
        self.dirty = true;
    }

    pub fn on_up(&mut self, x: f32, y: f32, cfg: &Config) -> SettingsEvent {
        let pressed = self.pressed.take();
        self.dirty = true;
        if let Some(s) = self.slider.take() {
            return SettingsEvent::Action(s.key.set(ConfigValue::Int(s.value as i64)));
        }
        let hit = self.hit_at(x, y).filter(|h| Some(*h) == pressed);
        // Clicking anywhere outside the field being edited commits it.
        if let Some(ed) = &self.editing {
            if hit != Some(Hit::Field(ed.key)) {
                return self.commit(cfg);
            }
        }
        let Some(hit) = hit else { return SettingsEvent::None };
        let action = match hit {
            Hit::Field(key) => {
                if self.editing.is_none() {
                    let mut field = TextField::new(&Self::text_value(key, cfg));
                    field.select_all();
                    self.editing = Some(Editing { key, field, invalid: false });
                }
                return SettingsEvent::None;
            }
            Hit::Close => return SettingsEvent::Close,
            Hit::Minimize => {
                self.minimize();
                return SettingsEvent::None;
            }
            Hit::Page(p) => {
                self.page = p;
                self.scroll = 0.0;
                return SettingsEvent::None;
            }
            Hit::Toggle(Key::StartAtLogin) => {
                self.login = !self.login;
                Action::SetStartAtLogin(self.login)
            }
            Hit::Toggle(key) => key.set(ConfigValue::Bool(!Self::toggle_on(self.login, key, cfg))),
            Hit::Segment(key, i) => {
                let (_, values, sel) = Self::segments(&self.fonts, key, cfg);
                if sel == Some(i) {
                    return SettingsEvent::None;
                }
                key.set(ConfigValue::Str(values[i].clone()))
            }
            Hit::Swatch(i) => Action::SetConfig {
                table: "general",
                key: "accent",
                value: ConfigValue::Str(ACCENTS[i].to_string()),
            },
            Hit::Remove(i) => {
                let mut apps = cfg.dock.apps.clone();
                if i < apps.len() {
                    apps.remove(i);
                }
                Action::SetDockApps(apps)
            }
            Hit::AddApp => Action::AddDockApp,
            Hit::OpenConfig => Action::OpenConfig,
            Hit::OpenLog => Action::Open(crate::config::dir().join("dockside.log").display().to_string()),
            Hit::ConnectClaude(on) => Action::ConnectClaude(on),
            Hit::Slider(_) => return SettingsEvent::None,
        };
        SettingsEvent::Action(action)
    }

    // --- drawing -----------------------------------------------------------

    pub fn render(&mut self, gfx: &Gfx, theme: &Theme, cfg: &Config, icons: &mut IconStore) {
        self.dirty = false;
        let dark = theme.dark;
        let card_bg = if dark { Color::rgba(44, 44, 46, 1.0) } else { Color::WHITE };
        let card_border = if dark { Color::gray(255, 0.07) } else { Color::gray(0, 0.07) };
        let control_bg = if dark { Color::gray(255, 0.1) } else { Color::gray(0, 0.06) };
        let selected_seg = if dark { Color::gray(255, 0.22) } else { Color::WHITE };
        let switch_off = if dark { Color::gray(255, 0.16) } else { Color::gray(0, 0.12) };

        let regular = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Leading);
        let right = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Trailing);
        let center = gfx.format(&theme.font, 12.0, Weight::Regular, Align::Center);
        let button_font = gfx.format(&theme.font, 13.0, Weight::Regular, Align::Center);
        let title_font = gfx.format(&theme.font, 15.0, Weight::Bold, Align::Leading);
        let header_font = gfx.format(&theme.font, 12.0, Weight::Bold, Align::Leading);
        let glyph_font = gfx.format("Segoe Fluent Icons", 12.0, Weight::Regular, Align::Center);

        // Icons for the pinned-apps list.
        if self.page == Page::Dock {
            for app in &cfg.dock.apps {
                let key = app.icon.clone().unwrap_or_else(|| app.path.clone());
                if !self.bitmaps.contains_key(&key) {
                    let bmp = icons
                        .get(&gfx.wic, &app.path, app.icon.as_deref())
                        .and_then(|px| px.to_bitmap(self.canvas.target()));
                    self.bitmaps.insert(key, bmp);
                }
            }
        }

        let groups = self.groups(cfg);
        let body = Self::body();
        let content_x = body.x + SIDEBAR + CONTENT_PAD;
        let content_w = body.w - SIDEBAR - 2.0 * CONTENT_PAD;
        let viewport = Rect::new(body.x + SIDEBAR, body.y + TITLE_H, body.w - SIDEBAR, body.h - TITLE_H);

        // Measure content height for scrolling.
        let content_h: f32 = groups
            .iter()
            .map(|g| if g.title.is_some() { 26.0 } else { 0.0 } + g.rows.len() as f32 * ROW_H + 20.0)
            .sum::<f32>()
            + 8.0;
        self.max_scroll = (content_h - viewport.h).max(0.0);
        self.scroll = self.scroll.min(self.max_scroll);

        let mut hits: Vec<(Rect, Hit)> = Vec::new();
        let (hover, page, scroll, slider) = (self.hover, self.page, self.scroll, self.slider);
        let page_title = PAGES.iter().find(|p| p.0 == page).map(|p| p.1).unwrap_or("");
        let (login, fonts, bitmaps, editing) = (self.login, &self.fonts, &self.bitmaps, &self.editing);
        let field_bg = if dark { Color::rgba(30, 30, 30, 1.0) } else { Color::WHITE };

        let r = {
            let draw = |p: &Painter| {
                chrome::window(p, body, SIDEBAR, theme, &chrome::Palette::new(theme));
                let group_hover = matches!(hover, Some(Hit::Close | Hit::Minimize));
                for (r, light) in chrome::traffic_lights(p, body, group_hover, false) {
                    hits.push((r, if light == chrome::Light::Close { Hit::Close } else { Hit::Minimize }));
                }

                // Sidebar.
                let mut y = body.y + TITLE_H;
                for (pg, name) in PAGES {
                    let row = Rect::new(body.x + 10.0, y, SIDEBAR - 20.0, 30.0);
                    let selected = pg == page;
                    if selected {
                        p.fill_round(row, 6.0, theme.accent);
                    } else if hover == Some(Hit::Page(pg)) {
                        p.fill_round(row, 6.0, control_bg);
                    }
                    let icon = Rect::new(row.x + 6.0, row.cy() - 10.0, 20.0, 20.0);
                    draw_page_icon(p, pg, icon, &glyph_font);
                    let text_c = if selected { Color::WHITE } else { theme.text };
                    p.text(name, &regular, Rect::new(icon.right() + 8.0, row.y, row.w - 40.0, row.h), text_c);
                    hits.push((row, Hit::Page(pg)));
                    y += 32.0;
                }

                // Toolbar title.
                p.text(page_title, &title_font, Rect::new(content_x, body.y, content_w, TITLE_H), theme.text);

                // Scrolling content.
                p.clipped(viewport, || {
                    let mut y = viewport.y + 8.0 - scroll;
                    for group in &groups {
                        if let Some(t) = group.title {
                            p.text(t, &header_font, Rect::new(content_x + 4.0, y, content_w, 26.0), theme.text_secondary);
                            y += 26.0;
                        }
                        let card = Rect::new(content_x, y, content_w, group.rows.len() as f32 * ROW_H);
                        p.fill_round(card, 10.0, card_bg);
                        p.stroke_round(card.inset(0.25, 0.25), 10.0, card_border, 0.5);
                        for (i, row) in group.rows.iter().enumerate() {
                            let r = Rect::new(card.x, y, card.w, ROW_H);
                            if i > 0 {
                                p.line(r.x + 14.0, r.y, r.right() - 14.0, r.y, card_border, 1.0);
                            }
                            let label_r = Rect::new(r.x + 14.0, r.y, r.w * 0.5, r.h);
                            let right_edge = r.right() - 14.0;
                            match row {
                                Row::Toggle(label, key) => {
                                    p.text(label, &regular, Rect::new(label_r.x, r.y, r.w - 80.0, r.h), theme.text);
                                    let on = Self::toggle_on(login, *key, cfg);
                                    let track = Rect::new(right_edge - 34.0, r.cy() - 10.0, 34.0, 20.0);
                                    p.fill_round(track, 10.0, if on { theme.accent } else { switch_off });
                                    let kx = if on { track.right() - 10.0 } else { track.x + 10.0 };
                                    p.fill_circle(kx, r.cy() + 0.5, 8.5, Color::gray(0, 0.12));
                                    p.fill_circle(kx, r.cy(), 8.5, Color::WHITE);
                                    hits.push((track.inset(-4.0, -6.0), Hit::Toggle(*key)));
                                }
                                Row::Segmented(label, key) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    let (labels, _, sel) = Self::segments(fonts, *key, cfg);
                                    let widths: Vec<f32> = labels.iter().map(|l| p.gfx.measure(l, &center) + 22.0).collect();
                                    let total: f32 = widths.iter().sum::<f32>() + 4.0;
                                    let outer = Rect::new(right_edge - total, r.cy() - 12.0, total, 24.0);
                                    p.fill_round(outer, 7.0, control_bg);
                                    let mut sx = outer.x + 2.0;
                                    for (i, (l, w)) in labels.iter().zip(&widths).enumerate() {
                                        let seg = Rect::new(sx, outer.y + 2.0, *w, 20.0);
                                        if sel == Some(i) {
                                            p.fill_round(seg.offset(0.0, 0.5), 5.0, Color::gray(0, 0.08));
                                            p.fill_round(seg, 5.0, selected_seg);
                                        } else if hover == Some(Hit::Segment(*key, i)) {
                                            p.fill_round(seg, 5.0, control_bg);
                                        }
                                        p.text(l, &center, seg, theme.text);
                                        hits.push((seg, Hit::Segment(*key, i)));
                                        sx += w;
                                    }
                                }
                                Row::Slider(label, key) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    let (mut value, min, max) = Self::slider_spec(*key, cfg);
                                    if let Some(s) = slider.filter(|s| s.key == *key) {
                                        value = s.value;
                                    }
                                    let track = Rect::new(right_edge - 240.0, r.cy() - 2.0, 240.0, 4.0);
                                    let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
                                    p.fill_round(track, 2.0, switch_off);
                                    p.fill_round(Rect::new(track.x, track.y, track.w * t, track.h), 2.0, theme.accent);
                                    let kx = track.x + track.w * t;
                                    p.fill_circle(kx, r.cy() + 0.5, 9.0, Color::gray(0, 0.15));
                                    p.fill_circle(kx, r.cy(), 9.0, Color::WHITE);
                                    let value_r = Rect::new(track.x - 60.0, r.y, 50.0, r.h);
                                    p.text(&format!("{}", value as i32), &right, value_r, theme.text_secondary);
                                    hits.push((Rect::new(track.x - 9.0, r.y, track.w + 18.0, r.h), Hit::Slider(*key)));
                                }
                                Row::Swatches(label) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    for (i, hex) in ACCENTS.iter().enumerate() {
                                        let c = Color::parse(hex).unwrap_or(theme.accent);
                                        let cx = right_edge - 9.0 - (ACCENTS.len() - 1 - i) as f32 * 26.0;
                                        let selected = cfg.general.accent.eq_ignore_ascii_case(hex);
                                        if selected {
                                            p.stroke_circle(cx, r.cy(), 11.5, c, 2.0);
                                        }
                                        p.fill_circle(cx, r.cy(), 8.5, c);
                                        if selected {
                                            p.fill_circle(cx, r.cy(), 3.0, Color::WHITE);
                                        }
                                        hits.push((Rect::new(cx - 12.0, r.cy() - 12.0, 24.0, 24.0), Hit::Swatch(i)));
                                    }
                                }
                                Row::App(i) => {
                                    if let Some(app) = cfg.dock.apps.get(*i) {
                                        let key = app.icon.clone().unwrap_or_else(|| app.path.clone());
                                        let icon = Rect::new(r.x + 12.0, r.cy() - 13.0, 26.0, 26.0);
                                        if let Some(Some(bmp)) = bitmaps.get(&key) {
                                            p.bitmap(bmp, icon, 1.0);
                                        }
                                        let name = app.name.clone().unwrap_or_else(|| app.path.clone());
                                        p.text(&name, &regular, Rect::new(icon.right() + 10.0, r.y, r.w * 0.45, r.h), theme.text);
                                        let path_r = Rect::new(r.x + r.w * 0.45, r.y, r.w * 0.55 - 52.0, r.h);
                                        p.text(&app.path, &right, path_r, theme.text_disabled);
                                        let (cx, cy) = (right_edge - 9.0, r.cy());
                                        let hot = hover == Some(Hit::Remove(*i));
                                        p.fill_circle(cx, cy, 9.0, if hot { Color::rgba(255, 82, 87, 1.0) } else { control_bg });
                                        p.line(cx - 4.0, cy, cx + 4.0, cy, if hot { Color::WHITE } else { theme.text_secondary }, 1.6);
                                        hits.push((Rect::new(cx - 11.0, cy - 11.0, 22.0, 22.0), Hit::Remove(*i)));
                                    }
                                }
                                Row::Buttons(label, buttons) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    let mut bx = right_edge;
                                    for (text, hit) in buttons.iter() {
                                        let bw = p.gfx.measure(text, &button_font) + 24.0;
                                        bx -= bw;
                                        let b = Rect::new(bx, r.cy() - 12.0, bw, 24.0);
                                        let bg = if hover == Some(*hit) { selected_seg } else { control_bg };
                                        p.fill_round(b, 6.0, bg);
                                        p.stroke_round(b.inset(0.25, 0.25), 6.0, card_border, 0.5);
                                        p.text(text, &button_font, b, theme.text);
                                        hits.push((b, *hit));
                                        bx -= 8.0;
                                    }
                                }
                                Row::Text(label, key, placeholder) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    let f = Rect::new(right_edge - 320.0, r.cy() - 12.0, 320.0, 24.0);
                                    p.fill_round(f, 6.0, field_bg);
                                    let ed = editing.as_ref().filter(|e| e.key == *key);
                                    let ring = match ed {
                                        Some(e) if e.invalid => Color::rgba(255, 69, 58, 1.0),
                                        Some(_) => theme.accent,
                                        None => card_border,
                                    };
                                    p.stroke_round(f.inset(0.5, 0.5), 6.0, ring, if ed.is_some() { 2.0 } else { 1.0 });
                                    let tr = Rect::new(f.x + 8.0, f.y, f.w - 16.0, f.h);
                                    match ed {
                                        Some(e) => {
                                            let prefix = |n: usize| p.gfx.measure(&e.field.text[..n].iter().collect::<String>(), &regular);
                                            let (a, b) = e.field.range();
                                            if a != b {
                                                let (x0, x1) = (prefix(a), prefix(b));
                                                p.fill_rect(Rect::new(tr.x + x0, f.y + 4.0, x1 - x0, f.h - 8.0), theme.accent.alpha(0.35));
                                            } else {
                                                let cx = tr.x + prefix(a) + 0.5;
                                                p.line(cx, f.y + 5.0, cx, f.bottom() - 5.0, theme.text, 1.0);
                                            }
                                            p.text(&e.field.string(), &regular, tr, theme.text);
                                        }
                                        None => {
                                            let mut value = Self::text_value(*key, cfg);
                                            // A calendar link is a secret: show where it points, not the link.
                                            if *key == Key::IslandIcs && !value.is_empty() {
                                                value = crate::island::calendar_links_summary(&value);
                                            }
                                            if value.is_empty() {
                                                p.text(placeholder, &regular, tr, theme.text_disabled);
                                            } else {
                                                p.text(&value, &regular, tr, theme.text);
                                            }
                                        }
                                    }
                                    hits.push((f, Hit::Field(*key)));
                                }
                                Row::Info(label, value) => {
                                    p.text(label, &regular, label_r, theme.text);
                                    let vr = Rect::new(r.x + r.w * 0.3, r.y, r.w * 0.7 - 14.0, r.h);
                                    p.text(value, &right, vr, theme.text_secondary);
                                }
                            }
                            y += ROW_H;
                        }
                        y += 20.0;
                    }
                });
            };
            self.canvas.draw(gfx, draw)
        };
        // Content hits outside the viewport (scrolled away) must not be clickable.
        hits.retain(|(r, h)| matches!(h, Hit::Close | Hit::Minimize | Hit::Page(_)) || (r.y >= viewport.y - 1.0 && r.bottom() <= viewport.bottom() + 1.0));
        self.hits = hits;

        let presented = r.and_then(|_| {
            if self.shown {
                self.canvas.present_in_place(self.hwnd)
            } else {
                let mut rc = RECT::default();
                unsafe { GetWindowRect(self.hwnd, &mut rc)? };
                self.canvas.present(self.hwnd, rc.left, rc.top)
            }
        });
        if let Err(e) = presented {
            log!("settings render failed: {e}");
        }
        if !self.shown {
            self.shown = true;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOW);
                let _ = SetForegroundWindow(self.hwnd);
            }
        }
    }
}

impl Drop for SettingsWindow {
    fn drop(&mut self) {
        SETTINGS_HWND.store(0, Ordering::Relaxed);
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Colored rounded-square page icons, like macOS System Settings.
fn draw_page_icon(p: &Painter, page: Page, r: Rect, glyph_font: &windows::Win32::Graphics::DirectWrite::IDWriteTextFormat) {
    let white = Color::WHITE;
    match page {
        Page::General => {
            p.fill_round(r, 5.0, Color::rgba(142, 142, 147, 1.0));
            p.text("\u{E713}", glyph_font, r, white);
        }
        Page::Appearance => {
            p.fill_round(r, 5.0, Color::rgba(28, 28, 30, 1.0));
            p.stroke_circle(r.cx(), r.cy(), 5.5, white, 1.4);
            p.clipped(Rect::new(r.cx(), r.y, r.w / 2.0, r.h), || p.fill_circle(r.cx(), r.cy(), 5.5, white));
        }
        Page::MenuBar => {
            p.fill_round(r, 5.0, Color::rgba(0, 122, 255, 1.0));
            let screen = r.inset(4.0, 5.0);
            p.stroke_round(screen, 1.5, white, 1.2);
            p.fill_rect(Rect::new(screen.x, screen.y, screen.w, 2.5), white);
        }
        Page::Island => {
            p.fill_round(r, 5.0, Color::rgba(28, 28, 30, 1.0));
            let pill = Rect::new(r.cx() - r.w * 0.3, r.y + r.h * 0.3, r.w * 0.6, r.h * 0.22);
            p.fill_round(pill, pill.h / 2.0, Color::rgba(0, 0, 0, 1.0));
            p.stroke_round(pill, pill.h / 2.0, white.alpha(0.55), 1.0);
            p.fill_circle(pill.right() - pill.h * 0.7, pill.cy(), pill.h * 0.22, Color::rgba(48, 209, 88, 1.0));
        }
        Page::Dock => {
            p.fill_round(r, 5.0, Color::rgba(28, 28, 30, 1.0));
            let screen = r.inset(4.0, 5.0);
            p.stroke_round(screen, 1.5, white, 1.2);
            p.fill_round(Rect::new(screen.cx() - 4.0, screen.bottom() - 3.5, 8.0, 2.0), 1.0, white);
        }
        Page::Desktops => {
            p.fill_round(r, 5.0, Color::rgba(88, 86, 214, 1.0));
            let (w, h) = (r.w * 0.5, r.h * 0.36);
            p.stroke_round(Rect::new(r.cx() - w / 2.0 + 2.0, r.cy() - h / 2.0 - 2.0, w, h), 1.2, white.alpha(0.6), 1.1);
            p.fill_round(Rect::new(r.cx() - w / 2.0 - 1.5, r.cy() - h / 2.0 + 1.5, w, h), 1.2, white);
        }
        Page::Finder => {
            // The two-tone Finder face.
            p.fill_round(r, 5.0, Color::rgba(30, 150, 255, 1.0));
            p.clipped(Rect::new(r.x, r.y, r.w * 0.5, r.h), || p.fill_round(r, 5.0, Color::rgba(120, 205, 255, 1.0)));
            let ink = Color::rgba(20, 40, 70, 1.0);
            p.line(r.x + r.w * 0.36, r.y + r.h * 0.3, r.x + r.w * 0.36, r.y + r.h * 0.45, ink, 1.3);
            p.line(r.x + r.w * 0.68, r.y + r.h * 0.3, r.x + r.w * 0.68, r.y + r.h * 0.45, ink, 1.3);
            p.line(r.x + r.w * 0.3, r.y + r.h * 0.66, r.x + r.w * 0.5, r.y + r.h * 0.72, ink, 1.2);
            p.line(r.x + r.w * 0.5, r.y + r.h * 0.72, r.x + r.w * 0.72, r.y + r.h * 0.64, ink, 1.2);
        }
    }
}
